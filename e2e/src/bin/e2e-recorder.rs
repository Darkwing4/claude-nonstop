use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [mode, dir] if mode == "hook" => record_hook(Path::new(dir)),
        [mode, log, behavior, argv @ ..] if mode == "call" => {
            record_call(Path::new(log), behavior, argv)
        }
        _ => {
            eprintln!(
                "usage: e2e-recorder hook <dir> | call <log.jsonl> <succeed|fail|hang> [argv...]"
            );
            ExitCode::from(2)
        }
    }
}

fn record_hook(dir: &Path) -> ExitCode {
    let payload = read_stdin();
    let event = serde_json::from_str::<Value>(&payload)
        .ok()
        .and_then(|value| value["hook_event_name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let pid = std::process::id();
    let partial = dir.join(format!(".{pid}.partial"));
    let target = dir.join(format!("{stamp}-{pid}-{event}.json"));

    std::fs::write(&partial, payload).expect("write the hook payload");
    std::fs::rename(&partial, &target).expect("publish the hook payload");

    ExitCode::SUCCESS
}

fn record_call(log: &Path, behavior: &str, argv: &[String]) -> ExitCode {
    let line = format!("{}\n", json!({ "argv": argv, "stdin": read_stdin() }));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("open the call log");
    file.write_all(line.as_bytes())
        .expect("append to the call log");
    drop(file);

    match behavior {
        "succeed" => ExitCode::SUCCESS,
        "fail" => ExitCode::FAILURE,
        "hang" => loop {
            std::thread::park();
        },
        other => {
            eprintln!("e2e-recorder: unknown behavior {other}");
            ExitCode::from(2)
        }
    }
}

fn read_stdin() -> String {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .expect("read stdin");

    text
}
