# Changelog

## [Unreleased]

## [0.1.0] - 2026-10-15

### Added

- `install` caps Claude Code's built-in retry countdown with `CLAUDE_CODE_MAX_RETRIES` in `~/.claude/settings.json` (1 by default, `--max-retries N`) and adds a StopFailure hook; nothing wraps the `claude` command.
- StopFailure hook that resumes cut-off turns, waits out overloads and an unreachable API on a 30 s to 5 min backoff with a 2 h cap, and records usage limits.
- Usage-limit tracking with reset-time parsing, `/rate-limit-options` menu answering, a transient resumer and reopening in tmux, zellij or headless.
- Optional daemon (systemd user unit or launchd agent) that also watches transcripts of every session.
- Notifications over notify-send, osascript, terminal OSC, bell and ntfy.
- Prompt queue (`prompt add --at`), `status`, `logs`, `resume-now`, `cancel`, `message`, `config`, `doctor`.
