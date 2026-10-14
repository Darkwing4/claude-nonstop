use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, SystemTime};

use rustix::event::kqueue::{
    Event, EventFilter, EventFlags, ProcessEvents, VnodeEvents, kevent, kqueue,
};
use rustix::fs::{Mode, OFlags};
use rustix::process::Pid;

use super::{SourceId, Wake};

const DEADLINE_IDENT: isize = 1;
const MAX_TIMER_SLICE: Duration = Duration::from_secs(30 * 60);
const MIN_TIMER_SLICE: Duration = Duration::from_millis(1);
const EVENT_BATCH: usize = 64;

type Snapshot = BTreeMap<OsString, (u64, SystemTime)>;

enum Source {
    File {
        fd: OwnedFd,
    },
    Dir {
        fd: OwnedFd,
        path: PathBuf,
        snapshot: Snapshot,
    },
    Process {
        pid: Pid,
    },
}

pub struct Reactor {
    queue: OwnedFd,
    deadline: Option<SystemTime>,
    next_id: u32,
    sources: BTreeMap<SourceId, Source>,
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let queue = kqueue()?;
        Ok(Reactor {
            queue,
            deadline: None,
            next_id: 1,
            sources: BTreeMap::new(),
        })
    }

    pub fn watch_file(&mut self, path: &Path) -> io::Result<SourceId> {
        let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
        self.add_vnode(
            &fd,
            VnodeEvents::WRITE | VnodeEvents::EXTEND | VnodeEvents::DELETE | VnodeEvents::RENAME,
        )?;
        Ok(self.register(Source::File { fd }))
    }

    pub fn watch_dir(&mut self, path: &Path) -> io::Result<SourceId> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        self.add_vnode(
            &fd,
            VnodeEvents::WRITE | VnodeEvents::DELETE | VnodeEvents::RENAME,
        )?;
        let snapshot = list_directory(path)?;
        Ok(self.register(Source::Dir {
            fd,
            path: path.to_path_buf(),
            snapshot,
        }))
    }

    pub fn watch_process(&mut self, pid: u32) -> io::Result<SourceId> {
        let pid = Pid::from_raw(pid as i32)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid is zero"))?;
        let event = Event::new(
            EventFilter::Proc {
                pid,
                flags: ProcessEvents::EXIT,
            },
            EventFlags::ADD | EventFlags::ONESHOT,
            ptr::null_mut(),
        );
        self.submit(event)?;
        Ok(self.register(Source::Process { pid }))
    }

    pub fn set_deadline(&mut self, at: Option<SystemTime>) -> io::Result<()> {
        self.deadline = at;
        match at {
            Some(at) => self.arm_timer(at),
            None => self.disarm_timer(),
        }
    }

    pub fn wait(&mut self) -> io::Result<Vec<Wake>> {
        let mut buffer = [MaybeUninit::<Event>::uninit(); EVENT_BATCH];
        let events: Vec<Event> = loop {
            match unsafe { kevent(&self.queue, &[], &mut buffer, None) } {
                Ok((ready, _)) => break ready.to_vec(),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let mut wakes = Vec::new();
        for event in events {
            match event.filter() {
                EventFilter::Vnode { vnode, flags } => {
                    self.translate_vnode(vnode, flags, &mut wakes)?
                }
                EventFilter::Proc { pid, flags } => {
                    if flags.intersects(ProcessEvents::EXIT) {
                        self.translate_process_exit(pid, &mut wakes);
                    }
                }
                EventFilter::Timer { ident, .. } if ident == DEADLINE_IDENT => {
                    self.translate_timer(&mut wakes)?
                }
                _ => {}
            }
        }
        Ok(wakes)
    }

    fn register(&mut self, source: Source) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id += 1;
        self.sources.insert(id, source);
        id
    }

    fn add_vnode(&self, fd: &OwnedFd, flags: VnodeEvents) -> io::Result<()> {
        let event = Event::new(
            EventFilter::Vnode {
                vnode: fd.as_raw_fd(),
                flags,
            },
            EventFlags::ADD | EventFlags::CLEAR,
            ptr::null_mut(),
        );
        self.submit(event)
    }

    fn submit(&self, change: Event) -> io::Result<()> {
        let mut none: [Event; 0] = [];
        loop {
            match unsafe { kevent(&self.queue, &[change], &mut none, Some(Duration::ZERO)) } {
                Ok(_) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn arm_timer(&self, at: SystemTime) -> io::Result<()> {
        let remaining = at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO)
            .clamp(MIN_TIMER_SLICE, MAX_TIMER_SLICE);
        let event = Event::new(
            EventFilter::Timer {
                ident: DEADLINE_IDENT,
                timer: Some(remaining),
            },
            EventFlags::ADD | EventFlags::ONESHOT,
            ptr::null_mut(),
        );
        self.submit(event)
    }

    fn disarm_timer(&self) -> io::Result<()> {
        let event = Event::new(
            EventFilter::Timer {
                ident: DEADLINE_IDENT,
                timer: None,
            },
            EventFlags::DELETE,
            ptr::null_mut(),
        );
        match self.submit(event) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn translate_vnode(
        &mut self,
        vnode: RawFd,
        flags: VnodeEvents,
        wakes: &mut Vec<Wake>,
    ) -> io::Result<()> {
        let found = self.sources.iter().find_map(|(id, source)| match source {
            Source::File { fd } | Source::Dir { fd, .. } if fd.as_raw_fd() == vnode => Some(*id),
            _ => None,
        });
        let Some(id) = found else {
            return Ok(());
        };
        if flags.intersects(VnodeEvents::DELETE | VnodeEvents::RENAME | VnodeEvents::REVOKE) {
            self.sources.remove(&id);
            wakes.push(Wake::FileGone(id));
            return Ok(());
        }
        match self.sources.get_mut(&id) {
            Some(Source::File { .. }) => {
                if flags.intersects(VnodeEvents::WRITE | VnodeEvents::EXTEND) {
                    wakes.push(Wake::FileWritten(id));
                }
            }
            Some(Source::Dir { path, snapshot, .. }) if flags.intersects(VnodeEvents::WRITE) => {
                let current = list_directory(path)?;
                for (name, stamp) in &current {
                    if snapshot.get(name) != Some(stamp) {
                        wakes.push(Wake::DirEntry(id, name.clone()));
                    }
                }
                *snapshot = current;
            }
            _ => {}
        }
        Ok(())
    }

    fn translate_process_exit(&mut self, pid: Pid, wakes: &mut Vec<Wake>) {
        let exited: Vec<SourceId> = self
            .sources
            .iter()
            .filter_map(|(id, source)| match source {
                Source::Process { pid: watched } if *watched == pid => Some(*id),
                _ => None,
            })
            .collect();
        for id in exited {
            self.sources.remove(&id);
            wakes.push(Wake::ProcessExited(id));
        }
    }

    fn translate_timer(&mut self, wakes: &mut Vec<Wake>) -> io::Result<()> {
        let Some(at) = self.deadline else {
            return Ok(());
        };
        if SystemTime::now() < at {
            return self.arm_timer(at);
        }
        self.deadline = None;
        wakes.push(Wake::Deadline);
        Ok(())
    }
}

impl AsFd for Reactor {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.queue.as_fd()
    }
}

fn list_directory(path: &Path) -> io::Result<Snapshot> {
    let mut snapshot = Snapshot::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        snapshot.insert(entry.file_name(), (metadata.len(), modified));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude-nonstop-reactor-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_write_wakes_with_file_written() {
        let dir = tempdir("file");
        let path = dir.join("log");
        fs::write(&path, "").unwrap();
        let mut reactor = Reactor::new().unwrap();
        let id = reactor.watch_file(&path).unwrap();
        fs::write(&path, "line\n").unwrap();
        let wakes = reactor.wait().unwrap();
        assert!(wakes.contains(&Wake::FileWritten(id)), "{wakes:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_removal_wakes_with_file_gone() {
        let dir = tempdir("gone");
        let path = dir.join("log");
        fs::write(&path, "").unwrap();
        let mut reactor = Reactor::new().unwrap();
        let id = reactor.watch_file(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let wakes = reactor.wait().unwrap();
        assert!(wakes.contains(&Wake::FileGone(id)), "{wakes:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_entry_wakes_with_its_name() {
        let dir = tempdir("dir");
        let mut reactor = Reactor::new().unwrap();
        let id = reactor.watch_dir(&dir).unwrap();
        fs::write(dir.join("marker.json"), "{}").unwrap();
        let wakes = reactor.wait().unwrap();
        assert!(wakes.iter().any(|wake| matches!(wake, Wake::DirEntry(seen, name) if *seen == id && name == "marker.json")), "{wakes:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn process_exit_wakes_with_process_exited() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let mut reactor = Reactor::new().unwrap();
        let id = reactor.watch_process(child.id()).unwrap();
        let wakes = reactor.wait().unwrap();
        assert_eq!(wakes, vec![Wake::ProcessExited(id)]);
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn deadline_wakes_after_it_passes() {
        let mut reactor = Reactor::new().unwrap();
        reactor
            .set_deadline(Some(SystemTime::now() + Duration::from_millis(50)))
            .unwrap();
        let started = std::time::Instant::now();
        let wakes = reactor.wait().unwrap();
        assert_eq!(wakes, vec![Wake::Deadline]);
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn deadline_in_the_past_wakes_immediately() {
        let mut reactor = Reactor::new().unwrap();
        reactor
            .set_deadline(Some(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(reactor.wait().unwrap(), vec![Wake::Deadline]);
    }

    #[test]
    fn disarming_an_unarmed_deadline_is_not_an_error() {
        let mut reactor = Reactor::new().unwrap();
        reactor.set_deadline(None).unwrap();
    }
}
