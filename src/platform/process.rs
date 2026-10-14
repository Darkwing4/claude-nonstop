use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, SystemTime};

use rustix::process::Pid;

use super::{Reactor, Wake};

pub fn alive(pid: u32) -> bool {
    let Some(pid) = Pid::from_raw(pid as i32) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(rustix::io::Errno::PERM) => true,
        Err(_) => false,
    }
}

pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    command.stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(io::Error::from)
        });
    }
    command.spawn()
}

pub fn wait_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    if let Some(status) = child.try_wait()? {
        return Ok(Some(status));
    }
    if timeout.is_zero() {
        return Ok(None);
    }
    let deadline = SystemTime::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "process wait deadline overflows",
        )
    })?;
    let mut reactor = Reactor::new()?;
    let source = match reactor.watch_process(child.id()) {
        Ok(source) => source,
        Err(error) => {
            return match child.try_wait()? {
                Some(status) => Ok(Some(status)),
                None => Err(error),
            };
        }
    };
    if let Some(status) = child.try_wait()? {
        return Ok(Some(status));
    }
    reactor.set_deadline(Some(deadline))?;
    loop {
        for wake in reactor.wait()? {
            match wake {
                Wake::ProcessExited(id) if id == source => return child.wait().map(Some),
                Wake::Deadline => return child.try_wait(),
                #[cfg(target_os = "linux")]
                Wake::ClockJumped => reactor.set_deadline(Some(deadline))?,
                _ => {}
            }
        }
    }
}

pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<CommandOutcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    match wait_timeout(&mut child, timeout)? {
        Some(status) => {
            let output = child.wait_with_output()?;
            Ok(CommandOutcome::Finished {
                status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
        None => {
            child.kill()?;
            child.wait()?;
            Ok(CommandOutcome::TimedOut)
        }
    }
}

#[derive(Debug)]
pub enum CommandOutcome {
    Finished {
        status: ExitStatus,
        stdout: String,
        stderr: String,
    },
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_timeout_returns_none_while_the_child_runs_and_status_after_exit() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(
            wait_timeout(&mut child, Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        child.kill().unwrap();
        assert!(
            wait_timeout(&mut child, Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn wait_timeout_returns_cached_status_for_reaped_child() {
        let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(
            wait_timeout(&mut child, Duration::ZERO).unwrap(),
            Some(status)
        );
        assert_eq!(
            wait_timeout(&mut child, Duration::from_secs(5)).unwrap(),
            Some(status)
        );
    }

    #[test]
    fn zero_timeout_leaves_running_child_alive() {
        let mut child = Command::new("sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait_timeout(&mut child, Duration::ZERO).unwrap().is_none());
        assert!(child.try_wait().unwrap().is_none());
        drop(child.stdin.take());
        assert!(
            wait_timeout(&mut child, Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn run_with_timeout_captures_output_or_kills() {
        match run_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(5)).unwrap() {
            CommandOutcome::Finished { stdout, .. } => assert_eq!(stdout, "hi\n"),
            CommandOutcome::TimedOut => panic!("echo timed out"),
        }
        assert!(matches!(
            run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(50)).unwrap(),
            CommandOutcome::TimedOut
        ));
    }

    #[test]
    fn detached_child_gets_its_own_session() {
        let mut child = spawn_detached(Command::new("sh").arg("-c").arg("kill -STOP $$")).unwrap();
        let pid = child.id();
        let child_pid = Pid::from_raw(pid as i32).unwrap();
        let session = rustix::process::getsid(Some(child_pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(session.unwrap(), child_pid);
        assert!(alive(std::process::id()));
        assert!(!alive(pid));
    }
}
