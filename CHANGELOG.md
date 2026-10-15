# Changelog

## [Unreleased]

### Removed

- `notify.channel` and notifications over terminal OSC and bell: they were never delivered. An existing `notify.channel` setting is ignored.
- `version` subcommand: use `claude-nonstop --version`.
- `name`, `mux_session`, `manual` and `env` keys of sessions and `mux_session` of prompts in `status --json` and `prompt list --json`. A session touched by `resume-now` now has `"reset_source": "manual"` instead of `"manual": true`.
- `SHA256SUMS` release asset: every archive still has its own `.sha256`.
- `install.sh` no longer accepts `uname -m` = `arm64` and no longer falls back to `shasum` when `sha256sum` is missing.

### Fixed

- `general.auto_resume = false` is no longer ignored: once a usage limit resets, the session waits for `resume-now` instead of being resumed.

## [0.1.0] - 2026-10-15

### Added

- `install` caps Claude Code's built-in retry countdown with `CLAUDE_CODE_MAX_RETRIES` in `~/.claude/settings.json` (1 by default, `--max-retries N`) and adds a StopFailure hook; nothing wraps the `claude` command.
- StopFailure hook that resumes cut-off turns, waits out overloads and an unreachable API on a 30 s to 5 min backoff with a 2 h cap, and records usage limits.
- Usage-limit tracking with reset-time parsing, `/rate-limit-options` menu answering, a transient resumer and reopening in tmux, zellij or headless.
- Optional daemon (systemd user unit) that also watches transcripts of every session.
- Notifications over notify-send, terminal OSC, bell and ntfy.
- Prompt queue (`prompt add --at`), `status`, `logs`, `resume-now`, `cancel`, `message`, `config`, `doctor`.
