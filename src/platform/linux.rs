use std::collections::BTreeMap;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use super::{SourceId, Wake};

enum Source {
    File { wd: i32 },
    Dir { wd: i32 },
    Process { fd: OwnedFd },
}

pub struct Reactor {
    inotify: OwnedFd,
    timer: OwnedFd,
    next_id: u32,
    sources: BTreeMap<SourceId, Source>,
}

impl Reactor {
    pub fn new() -> io::Result<Reactor> {
        let inotify = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)?;
        let timer = timerfd_create(
            TimerfdClockId::Realtime,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        Ok(Reactor {
            inotify,
            timer,
            next_id: 1,
            sources: BTreeMap::new(),
        })
    }

    pub fn watch_file(&mut self, path: &Path) -> io::Result<SourceId> {
        let wd = inotify::add_watch(
            &self.inotify,
            path,
            WatchFlags::MODIFY | WatchFlags::DELETE_SELF | WatchFlags::MOVE_SELF,
        )?;
        Ok(self.register(Source::File { wd }))
    }

    pub fn watch_dir(&mut self, path: &Path) -> io::Result<SourceId> {
        let flags = WatchFlags::CREATE
            | WatchFlags::MOVED_TO
            | WatchFlags::MODIFY
            | WatchFlags::DELETE_SELF;
        let wd = inotify::add_watch(&self.inotify, path, flags)?;
        Ok(self.register(Source::Dir { wd }))
    }

    pub fn watch_process(&mut self, pid: u32) -> io::Result<SourceId> {
        let pid = Pid::from_raw(pid as i32)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid is zero"))?;
        let fd = pidfd_open(pid, PidfdFlags::empty())?;
        Ok(self.register(Source::Process { fd }))
    }

    pub fn set_deadline(&mut self, at: Option<SystemTime>) -> io::Result<()> {
        let value = match at {
            Some(at) => {
                let since_epoch = at.duration_since(UNIX_EPOCH).unwrap_or_default();
                Timespec {
                    tv_sec: since_epoch.as_secs() as i64,
                    tv_nsec: i64::from(since_epoch.subsec_nanos()).max(1),
                }
            }
            None => Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: value,
        };
        timerfd_settime(
            &self.timer,
            TimerfdTimerFlags::ABSTIME | TimerfdTimerFlags::CANCEL_ON_SET,
            &spec,
        )?;
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<Vec<Wake>> {
        let mut ids: Vec<SourceId> = Vec::new();
        let mut fds: Vec<PollFd<'_>> = vec![
            PollFd::new(&self.inotify, PollFlags::IN),
            PollFd::new(&self.timer, PollFlags::IN),
        ];
        for (id, source) in &self.sources {
            if let Source::Process { fd } = source {
                ids.push(*id);
                fds.push(PollFd::new(fd, PollFlags::IN));
            }
        }
        loop {
            match poll(&mut fds, None) {
                Ok(_) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let ready: Vec<PollFlags> = fds.iter().map(PollFd::revents).collect();
        drop(fds);
        let mut wakes = Vec::new();
        if ready[0].intersects(PollFlags::IN) {
            self.drain_inotify(&mut wakes)?;
        }
        if ready[1].intersects(PollFlags::IN) {
            self.drain_timer(&mut wakes)?;
        }
        for (index, id) in ids.iter().enumerate() {
            if ready[index + 2].intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                self.sources.remove(id);
                wakes.push(Wake::ProcessExited(*id));
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

    fn drain_inotify(&mut self, wakes: &mut Vec<Wake>) -> io::Result<()> {
        let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
        loop {
            let mut reader = inotify::Reader::new(&self.inotify, &mut buffer);
            let mut events = Vec::new();
            loop {
                match reader.next() {
                    Ok(event) => {
                        let name = event.file_name().map(|name| {
                            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(
                                name.to_bytes(),
                            )
                            .to_owned()
                        });
                        events.push((event.wd(), event.events(), name));
                    }
                    Err(rustix::io::Errno::WOULDBLOCK) => break,
                    Err(error) => return Err(error.into()),
                }
            }
            if events.is_empty() {
                return Ok(());
            }
            for (wd, flags, name) in events {
                self.translate(wd, flags, name, wakes);
            }
        }
    }

    fn translate(
        &mut self,
        wd: i32,
        flags: ReadFlags,
        name: Option<std::ffi::OsString>,
        wakes: &mut Vec<Wake>,
    ) {
        let found = self.sources.iter().find_map(|(id, source)| match source {
            Source::File { wd: watched } | Source::Dir { wd: watched } if *watched == wd => {
                Some((*id, matches!(source, Source::Dir { .. })))
            }
            _ => None,
        });
        let Some((id, is_dir)) = found else {
            return;
        };
        if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED) {
            self.sources.remove(&id);
            wakes.push(Wake::FileGone(id));
            return;
        }
        match (is_dir, name) {
            (true, Some(name)) => wakes.push(Wake::DirEntry(id, name)),
            (true, None) => {}
            (false, _) => wakes.push(Wake::FileWritten(id)),
        }
    }

    fn drain_timer(&mut self, wakes: &mut Vec<Wake>) -> io::Result<()> {
        let mut expirations = [0u8; 8];
        match rustix::io::read(&self.timer, &mut expirations) {
            Ok(_) => wakes.push(Wake::Deadline),
            Err(rustix::io::Errno::CANCELED) => wakes.push(Wake::ClockJumped),
            Err(rustix::io::Errno::WOULDBLOCK) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

impl AsFd for Reactor {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.inotify.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

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
}
