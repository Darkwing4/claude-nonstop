use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use serde_json::Value;

use super::Pane;

const SUBSCRIBE_ATTEMPTS: usize = 5;

pub struct PaneFeed {
    pane: Pane,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

struct Shared {
    viewport: Mutex<String>,
    subscriber: Mutex<Subscriber>,
}

#[derive(Default)]
struct Subscriber {
    child: Option<Child>,
    stopping: bool,
}

enum StreamEnd {
    PaneClosed,
    Disconnected,
}

impl PaneFeed {
    pub fn follow(
        pane: &Pane,
        subscribe: impl Fn() -> Command + Send + 'static,
        feed: &Path,
    ) -> PaneFeed {
        let errors = append_to(&feed.with_extension("feed.err"));
        let feed = append_to(feed);
        let shared = Arc::new(Shared {
            viewport: Mutex::new(String::new()),
            subscriber: Mutex::new(Subscriber::default()),
        });
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || keep_subscribed(&shared, subscribe, feed, errors))
        };

        PaneFeed {
            pane: pane.clone(),
            shared,
            reader: Some(reader),
        }
    }

    pub fn pane(&self) -> &Pane {
        &self.pane
    }

    pub fn viewport(&self) -> String {
        lock(&self.shared.viewport).clone()
    }

    pub fn stop(&mut self) {
        {
            let mut subscriber = lock(&self.shared.subscriber);
            subscriber.stopping = true;

            if let Some(child) = subscriber.child.as_mut() {
                if let Err(error) = child.kill() {
                    eprintln!("e2e: zellij subscribe to {} not killed: {error}", self.pane);
                }
            }
        }

        if let Some(reader) = self.reader.take() {
            if reader.join().is_err() {
                eprintln!("e2e: zellij subscribe reader of {} panicked", self.pane);
            }
        }
    }
}

fn keep_subscribed(shared: &Shared, subscribe: impl Fn() -> Command, mut feed: File, errors: File) {
    for _ in 0..SUBSCRIBE_ATTEMPTS {
        let Some(stdout) = start_subscriber(shared, &subscribe, &errors) else {
            return;
        };
        let end = forward_updates(stdout, shared, &mut feed);
        let stopping = reap_subscriber(shared);

        if stopping || matches!(end, StreamEnd::PaneClosed) {
            return;
        }
    }
}

fn start_subscriber(
    shared: &Shared,
    subscribe: &impl Fn() -> Command,
    errors: &File,
) -> Option<ChildStdout> {
    let mut subscriber = lock(&shared.subscriber);

    if subscriber.stopping {
        return None;
    }

    let stderr = errors
        .try_clone()
        .expect("share the zellij subscribe error log");
    let mut child = subscribe()
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("start zellij subscribe");
    let stdout = child
        .stdout
        .take()
        .expect("zellij subscribe stdout is piped");
    subscriber.child = Some(child);

    Some(stdout)
}

fn forward_updates(stdout: ChildStdout, shared: &Shared, feed: &mut File) -> StreamEnd {
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("e2e: zellij subscribe output unreadable: {error}");
                return StreamEnd::Disconnected;
            }
        };
        let update: Value = match serde_json::from_str(&line) {
            Ok(update) => update,
            Err(error) => panic!("zellij subscribe printed non-JSON {line:?}: {error}"),
        };

        if update["event"] == "pane_closed" {
            return StreamEnd::PaneClosed;
        }

        if let Some(rows) = update["viewport"].as_array() {
            let rows: Vec<&str> = rows.iter().filter_map(Value::as_str).collect();
            *lock(&shared.viewport) = rows.join("\n");
        }

        if let Err(error) = writeln!(feed, "{line}") {
            eprintln!("e2e: zellij subscribe output not saved: {error}");
        }
    }

    StreamEnd::Disconnected
}

fn reap_subscriber(shared: &Shared) -> bool {
    let (child, stopping) = {
        let mut subscriber = lock(&shared.subscriber);
        (subscriber.child.take(), subscriber.stopping)
    };

    if let Some(mut child) = child {
        if let Err(error) = child.wait() {
            eprintln!("e2e: zellij subscribe not reaped: {error}");
        }
    }

    stopping
}

fn append_to(path: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
