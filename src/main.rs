mod agent;
mod at_time;
mod atomic_file;
mod claude_sessions;
mod cli;
mod config;
mod continue_counter;
mod doctor;
mod error;
mod hook;
mod install_daemon;
mod install_hook;
mod install_retry_limit;
mod ledger;
mod limit_detect;
mod log;
mod logs;
mod mux;
mod notify;
mod overload_ladder;
mod pane_input;
mod paths;
mod platform;
mod prompt_queue;
mod recover;
mod reopen;
mod reset_time;
mod resumer;
mod screen;
mod status;
mod transcript_claude;
mod transcript_watch;

use clap::Parser;

use crate::cli::{Cli, Command, ConfigAction, PromptAction, TargetArgs};
use crate::error::{Error, Result};
use crate::paths::Paths;

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(error) => {
            log::line("", error.to_string());
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let paths = Paths::from_env()?;
    match cli.command {
        Command::Daemon => {
            paths.ensure_state_dirs()?;
            log::init(paths.log_file(), "daemon", true);
            let loaded = config::load(&paths)?;
            resumer::run(&paths, &loaded.config, true)
        }
        Command::Resumer => {
            paths.ensure_state_dirs()?;
            log::init(paths.log_file(), "resumer", false);
            let loaded = config::load(&paths)?;
            resumer::run(&paths, &loaded.config, false)
        }
        Command::Hook => {
            paths.ensure_state_dirs()?;
            log::init(paths.log_file(), "hook", false);
            let loaded = config::load(&paths)?;
            hook::run(&paths, &loaded.config, &mut std::io::stdin())
        }
        Command::Install {
            daemon,
            max_retries,
        } => {
            config::load(&paths)?;
            paths.ensure_state_dirs()?;
            let binary = std::env::current_exe()
                .map_err(|source| Error::io("claude-nonstop", "resolve", source))?;
            for line in install_hook::install(&paths, &binary)? {
                println!("{line}");
            }
            for line in install_retry_limit::install(&paths, max_retries)? {
                println!("{line}");
            }
            if daemon {
                for line in install_daemon::install(&paths, &binary)? {
                    println!("{line}");
                }
            }
            println!("new Claude Code sessions pick up the hook and the retry limit");
            Ok(0)
        }
        Command::Uninstall { purge } => {
            for line in install_hook::uninstall(&paths)? {
                println!("{line}");
            }
            for line in install_retry_limit::uninstall(&paths)? {
                println!("{line}");
            }
            for line in install_daemon::uninstall(&paths)? {
                println!("{line}");
            }
            if purge {
                if paths.state.is_dir() {
                    std::fs::remove_dir_all(&paths.state)
                        .map_err(|source| Error::io(&paths.state, "remove", source))?;
                    println!("removed {}", paths.state.display());
                }
                if paths.config_file.is_file() {
                    std::fs::remove_file(&paths.config_file)
                        .map_err(|source| Error::io(&paths.config_file, "remove", source))?;
                    println!("removed {}", paths.config_file.display());
                }
            }
            Ok(0)
        }
        Command::Status { json } => {
            print!("{}", status::status(&paths, json)?);
            Ok(0)
        }
        Command::Logs { follow, lines } => logs::show(&paths, lines, follow),
        Command::ResumeNow(target) => {
            log::init(paths.log_file(), "cli", false);
            let count = status::resume_now(&paths, &target_of(target))?;
            println!("{count} session(s) woken");
            Ok(if count == 0 { 1 } else { 0 })
        }
        Command::Cancel(target) => {
            let count = status::cancel(&paths, &target_of(target))?;
            println!("{count} session(s) cancelled");
            Ok(if count == 0 { 1 } else { 0 })
        }
        Command::Message {
            target,
            clear,
            text,
        } => {
            let message = if clear { None } else { Some(text.join(" ")) };
            if message.as_deref().is_some_and(|m| m.trim().is_empty()) {
                return Err(Error::Invalid("give a message or --clear".to_owned()));
            }
            let count = status::set_message(&paths, &target_of(target), message)?;
            println!("{count} session(s) updated");
            Ok(if count == 0 { 1 } else { 0 })
        }
        Command::Prompt { action } => prompt_command(&paths, action),
        Command::Doctor(args) => {
            let findings = doctor::run(&paths, args.fix)?;
            if args.json {
                let report = serde_json::json!({ "version": 1, "findings": findings });
                print_json("doctor", &report)?;
            } else {
                for finding in &findings {
                    let mark = if finding.fixed {
                        "fixed"
                    } else {
                        severity_label(finding.severity)
                    };
                    println!("[{mark}] {}: {}", finding.id, finding.title);
                    if let Some(hint) = &finding.hint {
                        println!("        {hint}");
                    }
                }
            }
            Ok(if doctor::worst(&findings) == doctor::Severity::Error {
                1
            } else {
                0
            })
        }
        Command::Config { action } => config_command(&paths, action),
    }
}

fn target_of(args: TargetArgs) -> status::Target {
    match args.id {
        Some(id) => status::Target::Id(id),
        None => status::Target::All,
    }
}

fn prompt_command(paths: &Paths, action: PromptAction) -> Result<i32> {
    match action {
        PromptAction::Add {
            agent,
            project,
            at,
            now,
            text,
        } => {
            paths.ensure_state_dirs()?;
            log::init(paths.log_file(), "cli", false);
            let loaded = config::load(paths)?;
            let cwd = prompt_queue::cwd_or_current(project)?;
            let (when, at_unix) = if now {
                (ledger::PromptWhen::Now, None)
            } else if let Some(text) = at {
                let parsed = at_time::parse_at(
                    &text,
                    jiff::Timestamp::now(),
                    &jiff::tz::TimeZone::system(),
                )?;
                (ledger::PromptWhen::At, Some(parsed))
            } else {
                (ledger::PromptWhen::NextReset, None)
            };
            let entry = prompt_queue::add(
                paths,
                &loaded.config,
                agent.into(),
                &cwd,
                &text.join(" "),
                when,
                at_unix,
            )?;
            println!(
                "{}  {}  {}",
                entry.id,
                prompt_queue::describe_when(&entry),
                entry.cwd.display()
            );
            Ok(0)
        }
        PromptAction::List { json } => {
            let items = ledger::read(paths)?.prompts;
            if json {
                let report = serde_json::json!({ "version": 1, "prompts": items });
                print_json("prompts", &report)?;
            } else {
                for item in &items {
                    let preview: String = item.prompt.chars().take(60).collect();
                    println!(
                        "{}  {:<6} {:<12} {:?}  {}  {preview:?}",
                        item.id,
                        item.agent.name(),
                        prompt_queue::describe_when(item),
                        item.status,
                        item.cwd.display()
                    );
                }
            }
            Ok(0)
        }
        PromptAction::Remove { id } => {
            let removed = prompt_queue::remove(paths, &id)?;
            println!(
                "{}",
                if removed {
                    "removed"
                } else {
                    "no pending prompt with that id"
                }
            );
            Ok(if removed { 0 } else { 1 })
        }
        PromptAction::Clear => {
            let count = prompt_queue::clear(paths)?;
            println!("{count} prompt(s) removed");
            Ok(0)
        }
    }
}

fn config_command(paths: &Paths, action: ConfigAction) -> Result<i32> {
    match action {
        ConfigAction::Path => {
            println!("{}", paths.config_file.display());
            Ok(0)
        }
        ConfigAction::Set { key, value } => {
            config::set(paths, &key, &value)?;
            let loaded = config::load(paths)?;
            println!("{key} = {}", config::get(&loaded, &key)?);
            Ok(0)
        }
        ConfigAction::Get { key } => {
            let loaded = config::load(paths)?;
            println!("{}", config::get(&loaded, &key)?);
            Ok(0)
        }
        ConfigAction::List { json } => {
            let loaded = config::load(paths)?;
            let rows = config::list(&loaded)?;
            if json {
                let entries: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(key, value, source)| serde_json::json!({ "key": key, "value": value, "source": source_label(*source) }))
                    .collect();
                let report = serde_json::json!({ "version": 1, "path": paths.config_file, "entries": entries });
                print_json("config", &report)?;
            } else {
                let width = rows.iter().map(|(key, _, _)| key.len()).max().unwrap_or(0);
                for (key, value, source) in &rows {
                    println!("{key:<width$}  {value}  ({})", source_label(*source));
                }
            }
            Ok(0)
        }
    }
}

fn print_json(label: &str, report: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(report).map_err(|source| Error::Json {
        path: label.into(),
        source,
    })?;
    println!("{text}");
    Ok(())
}

fn severity_label(severity: doctor::Severity) -> &'static str {
    match severity {
        doctor::Severity::Info => "ok",
        doctor::Severity::Warn => "warn",
        doctor::Severity::Error => "error",
    }
}

fn source_label(source: config::ValueSource) -> &'static str {
    match source {
        config::ValueSource::Default => "default",
        config::ValueSource::File => "file",
        config::ValueSource::Env => "env",
    }
}
