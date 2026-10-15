use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;

use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

pub struct ZellijClient {
    child: Child,
}

impl ZellijClient {
    pub fn attach(mut zellij: Command, columns: u16, rows: u16, output: &Path) -> ZellijClient {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
            .expect("open a pseudo terminal");
        grantpt(&master).expect("grant the pseudo terminal");
        unlockpt(&master).expect("unlock the pseudo terminal");
        tcsetwinsize(
            &master,
            Winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("size the pseudo terminal");

        let name = ptsname(&master, Vec::new()).expect("name the pseudo terminal");
        let terminal = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NOCTTY.bits() as i32)
            .open(name.to_str().expect("pty name is UTF-8"))
            .expect("open the pseudo terminal");
        let stdio = |file: &File| Stdio::from(file.try_clone().expect("share the pseudo terminal"));

        let child = zellij
            .stdin(stdio(&terminal))
            .stdout(stdio(&terminal))
            .stderr(stdio(&terminal))
            .spawn()
            .expect("start the zellij client");
        drop(terminal);

        let sink = OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .unwrap_or_else(|error| panic!("open {}: {error}", output.display()));
        thread::spawn(move || copy_until_hangup(File::from(master), sink));

        ZellijClient { child }
    }

    pub fn stop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("e2e: zellij client not killed: {error}");
        }

        if let Err(error) = self.child.wait() {
            eprintln!("e2e: zellij client not reaped: {error}");
        }
    }
}

fn copy_until_hangup(mut master: File, mut sink: File) {
    let mut buffer = [0u8; 16 * 1024];

    loop {
        let read = match master.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.raw_os_error() == Some(Errno::IO.raw_os_error()) => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("e2e: zellij client output unreadable: {error}");
                return;
            }
        };

        if let Err(error) = sink.write_all(&buffer[..read]) {
            eprintln!("e2e: zellij client output not saved: {error}");
            return;
        }
    }
}
