use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

pub struct Waiter {
    inotify: OwnedFd,
    dirs: RefCell<HashMap<i32, PathBuf>>,
}

impl Waiter {
    pub fn watching(root: &Path) -> Waiter {
        let inotify = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .expect("inotify is available");
        let waiter = Waiter {
            inotify,
            dirs: RefCell::new(HashMap::new()),
        };
        waiter.watch_tree(root);

        waiter
    }

    pub fn until<T>(
        &self,
        what: &str,
        timeout: Duration,
        mut check: impl FnMut() -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(found) = check() {
                return found;
            }

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("timed out after {}s waiting for {what}", timeout.as_secs());
            }

            self.next_change(left);
        }
    }

    fn next_change(&self, timeout: Duration) {
        let timeout = Timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&self.inotify, PollFlags::IN)];

        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => panic!("poll on inotify failed: {error}"),
        }

        self.drain();
    }

    fn drain(&self) {
        let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
        let mut created_dirs = Vec::new();
        let mut reader = inotify::Reader::new(&self.inotify, &mut buffer);

        loop {
            match reader.next() {
                Ok(event) => {
                    let is_new_dir = event.events().contains(ReadFlags::ISDIR)
                        && event
                            .events()
                            .intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO);
                    let parent = self.dirs.borrow().get(&event.wd()).cloned();

                    if let (true, Some(parent), Some(name)) =
                        (is_new_dir, parent, event.file_name())
                    {
                        created_dirs.push(parent.join(OsStr::from_bytes(name.to_bytes())));
                    }
                }
                Err(Errno::WOULDBLOCK) => break,
                Err(error) => panic!("reading inotify events failed: {error}"),
            }
        }

        for dir in created_dirs {
            self.watch_tree(&dir);
        }
    }

    fn watch_tree(&self, dir: &Path) {
        let flags = WatchFlags::CREATE
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_TO
            | WatchFlags::DELETE
            | WatchFlags::ONLYDIR;

        match inotify::add_watch(&self.inotify, dir, flags) {
            Ok(wd) => {
                self.dirs.borrow_mut().insert(wd, dir.to_path_buf());
            }
            Err(Errno::NOENT | Errno::NOTDIR) => return,
            Err(error) => panic!("cannot watch {}: {error}", dir.display()),
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("cannot list {}: {error}", dir.display()),
        };

        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir {
                self.watch_tree(&entry.path());
            }
        }
    }
}
