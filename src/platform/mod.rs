use std::ffi::OsString;

pub mod process;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Reactor;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Reactor;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    DirEntry(SourceId, OsString),
    ProcessExited(SourceId),
    Deadline,
    #[cfg(target_os = "linux")]
    ClockJumped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn directory_entry_wakes_with_its_name() {
        let dir =
            std::env::temp_dir().join(format!("claude-nonstop-reactor-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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
        let started = Instant::now();
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
