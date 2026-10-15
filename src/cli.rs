use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::ledger::AgentId;

#[derive(Parser)]
#[command(
    name = "claude-nonstop",
    version,
    about = "Keeps Claude Code sessions running through retries, cut-off turns, overloads and usage limits",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(about = "Install the Claude Code hook and the retry limit")]
    Install {
        #[arg(
            long,
            help = "Also install and start the daemon (systemd user unit or launchd agent)"
        )]
        daemon: bool,
        #[arg(
            long,
            value_parser = clap::value_parser!(u8).range(0..=15),
            help = "How many times Claude Code retries a failed request before giving up to claude-nonstop (default 1, an existing value is kept)"
        )]
        max_retries: Option<u8>,
    },
    #[command(about = "Remove the hook and the retry limit")]
    Uninstall {
        #[arg(
            long,
            help = "Also delete the state directory and the configuration file"
        )]
        purge: bool,
    },
    #[command(about = "Show waiting sessions and queued prompts")]
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Print the log")]
    Logs {
        #[arg(short, long, help = "Keep printing new lines")]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    #[command(name = "resume-now", about = "Wake a waiting session right away")]
    ResumeNow(TargetArgs),
    #[command(about = "Stop tracking a waiting session")]
    Cancel(TargetArgs),
    #[command(about = "Set or clear the message typed when a session resumes")]
    Message {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, conflicts_with = "text")]
        clear: bool,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    #[command(
        about = "Queue a prompt for a new session at a given time or after the next limit reset"
    )]
    Prompt {
        #[command(subcommand)]
        action: PromptAction,
    },
    #[command(about = "Check the installation and report what needs attention")]
    Doctor(DoctorArgs),
    #[command(about = "Read or change the configuration")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(hide = true)]
    Hook,
    #[command(hide = true)]
    Resumer,
    #[command(about = "Run the resumer in the foreground and watch transcripts of every session")]
    Daemon,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AgentArg {
    Claude,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_claude_can_be_selected() {
        assert!(
            Cli::try_parse_from(["claude-nonstop", "prompt", "add", "--agent", "claude", "hi"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "claude-nonstop",
                "prompt",
                "add",
                "--agent",
                "codex",
                "hello"
            ])
            .is_err()
        );
    }
}

impl From<AgentArg> for AgentId {
    fn from(value: AgentArg) -> AgentId {
        match value {
            AgentArg::Claude => AgentId::Claude,
        }
    }
}

#[derive(Args)]
pub struct DoctorArgs {
    #[arg(long, help = "Apply the fixes doctor knows how to make")]
    pub fix: bool,
    #[arg(long, help = "Print findings as JSON")]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    #[command(about = "Print every key with its effective value and where it comes from")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Print the effective value of one key")]
    Get { key: String },
    #[command(about = "Write one key into the configuration file")]
    Set { key: String, value: String },
    #[command(about = "Print the configuration file path")]
    Path,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct TargetArgs {
    #[arg(help = "Session id or its prefix")]
    pub id: Option<String>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum PromptAction {
    #[command(about = "Add a prompt")]
    Add {
        #[arg(long, default_value = "claude")]
        agent: AgentArg,
        #[arg(long, help = "Project directory, defaults to the current one")]
        project: Option<PathBuf>,
        #[arg(
            long,
            conflicts_with = "now",
            help = "+2h30m, 14:30, 7pm, an ISO date or an epoch"
        )]
        at: Option<String>,
        #[arg(long, help = "Start right away")]
        now: bool,
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    #[command(about = "List queued prompts")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Remove one prompt")]
    Remove { id: String },
    #[command(about = "Remove every pending prompt")]
    Clear,
}
