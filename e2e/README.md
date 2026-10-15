# End-to-end scenarios

Real Claude Code in a real tmux or zellij pane, a fake Anthropic API, a real
`claude-nonstop` build. Everything runs in Docker with `--network none`; nothing
touches your `~/.claude`, `~/.claude.json` or multiplexer sessions.

## Run

```
e2e/run.sh                 # whole matrix
e2e/run.sh s0_             # one group: any libtest name filter
e2e/run.sh --exact b1_turn_cut_off_by_server_errors_is_continued_tmux
e2e/run.sh --skip _slow_   # everything but the slow scenarios
```

Needs Docker and a Claude Code binary in `~/.local/share/claude/versions/`
(newest is used, `CLAUDE_CODE_VERSION=2.1.300 e2e/run.sh` picks one). The first
run builds the `claude-nonstop-e2e` image (rust slim + tmux + zellij 0.45.1,
x86_64) and needs network for that; `cargo fetch` runs on the host, the
container builds offline into `target/e2e`.

Test names start with the matrix id (`s0_`, `b1_`, `d1_`) and end with the
multiplexer (`_tmux`, `_zellij`), so both work as filters. A scenario that has
to sit out a delay claude-nonstop does not let us shorten (the 60 s typing
backoff, `limit.probe_minutes` of at least a minute, the 120 s limit episode
window) has `slow` right after the matrix id, `d5_slow_…`:
`e2e/run.sh _slow_` runs only those, `--skip _slow_` leaves them out.

## Failures

A failing scenario keeps its sandbox in `target/e2e/artifacts/<test name>/` and
prints the path, the last lines of the Claude pane and of the claude-nonstop
log. Inside:

| path | what |
|---|---|
| `screens/*.txt` | pane scrollback at the moment of failure |
| `screens/*.feed` | pane output: raw bytes from tmux `pipe-pane`, viewport JSON lines from `zellij subscribe` (its stderr in `*.feed.err`) |
| `claude-debug.log` | Claude Code `--debug-file` |
| `mock/requests.jsonl` | every API request with `last_user_text` and the reply it got |
| `hooks/` | StopFailure, Stop and SessionStart payloads in order |
| `calls/` | fake `notify-send`, `curl`, `systemctl` invocations |
| `home/.claude/settings.json`, `home/.claude/projects/` | settings and transcripts |
| `home/.local/state/claude-nonstop/` | `ledger.json`, `cutoff/`, `log/claude-nonstop.log` |

A message that starts with `KNOWN BUG <n>:` is the product misbehaving as
listed in [Known product bugs](#known-product-bugs), not the harness.
`timed out after Ns waiting for X` names the event that never happened; the
artifacts show how far it got.

## Known product bugs

Scenarios are written for the right behaviour, so these bugs fail them on
purpose; the failure message starts with `KNOWN BUG <n>:`.

| # | bug | caught by |
|---|---|---|
| 1 | a 529 arrives as `server_error` and is continued as a cut-off turn instead of waited out as an overload | `c1_overloaded_api_is_waited_out_tmux`, `c2_overload_that_never_ends_is_given_up_tmux` |
| 2 | `claude -p` kills the StopFailure hook on exit, so the limit of a headless run is never recorded | `f4_headless_prompt_that_hits_a_limit_records_it` |
| 3 | Claude Code's own auto-continue races the resumer; a five-hour reset must continue exactly once | `d1_five_hour_limit_resumes_once_after_reset_tmux`, `d1_slow_five_hour_limit_resumes_once_after_reset_zellij` |
| 4 | the reset time is read only from the transcript, not yet flushed when the hook runs; `last_assistant_message` is ignored | `d1_five_hour_limit_resumes_once_after_reset_tmux` |
| 5 | the hook reads the pane the moment the turn ends, still sees the busy footer or the prompt just sent and gives up without a retry (flaky) | `b1_*`, `b2_*`, `b3_*`, `b4_*`, `c8_*` |
| 6 | `general.multiplexer=auto` reopens a zellij session in tmux whenever tmux is on PATH | `d6_closed_pane_is_reopened_after_the_limit_zellij` |
| 7 | the daemon registers a project directory that appears while it is busy at its current size and misses the limit already in it | `e3_daemon_reads_a_project_directory_created_while_it_was_busy` |
| 8 | a hanging `notify-send` uses up the 5 s hook timeout before the resumer is spawned | `h3_hook_cancelled_over_a_hanging_notify_send_still_leaves_a_resumer_tmux` |
| 9 | the hook looks for the `/rate-limit-options` menu before Claude Code draws it | `d2_weekly_limit_menu_is_answered_as_soon_as_it_opens_tmux` |
| 10 | right after answering the menu the resumer takes the menu options for typed text and backs off (−60 s and an attempt) | `d2_five_hour_limit_menu_is_answered_before_the_resume_tmux` |
| 11 | a limit hit by the resume itself is lost: folded into the old episode, and verify compares whole seconds | `d3_slow_limit_hit_again_after_the_resume_is_recorded_anew_tmux` |
| 12 | a started queued prompt is verified by one screen capture a second after sending; a limit banner drawn later leaves it delivered and never retried (flaky) | `f3_slow_prompt_that_hits_a_limit_at_start_is_retried_tmux`, `f3_slow_prompt_that_hits_a_limit_at_start_is_retried_zellij` |

## Writing a scenario

```rust
let mut scenario = Scenario::in_tmux()
    .replies([Reply::server_error(), Reply::server_error()])
    .start();
scenario.launch_claude();

scenario.submit("say ok");

scenario.wait_for_log("resume 1/3 written to", TURN);
```

- Scripted replies go to main-loop requests (with tools) in order; after the
  script runs out every request gets the fallback (`Reply::ok("ok")` unless
  `fallback_reply` changes it). Side requests such as title generation always
  get `ok`.
- Waiting is event driven: every `wait_for_*` re-checks its condition on any
  file change in the sandbox (inotify), the timeout is only the failure bound.
  Do not sleep.
- zellij 0.45.1 hands a freed client id to the next connection while the
  cleanup of its previous owner is still queued (fixed upstream by monotonic
  ids in zellij-org/zellij#5630, not released yet), so under load a CLI call
  can fail its session lookup with `There is no active session!` or lose its
  reply. The zellij driver therefore reads screens from one `zellij subscribe`
  stream per pane instead of calling `dump-screen`, resubscribes a stream that
  ends while its pane is open, and repeats a call (3 attempts, no delay) only
  when the session lookup failed or a read-only query printed nothing; a lost
  `zellij run` reply is recovered from `list-panes`.
- `claude-nonstop` runs with `LIMIT_MARGIN_SECONDS=0`, `LIMIT_VERIFY_SECONDS=1`,
  `OVERLOAD_BACKOFF_SECONDS=1,2`, `OVERLOAD_JITTER_PERCENT=0`; anything else goes
  through `.env(..)`. An unknown `CLAUDE_NONSTOP_*` variable breaks every command.
- Claude Code does not pass `CLAUDE_CODE_OAUTH_TOKEN` to its hooks, so a
  subscription Claude Code that claude-nonstop starts from a hook's resumer (a
  reopened session, a queued prompt) is not logged in. `.login_in_settings()`
  also stores the credentials in the `env` of `settings.json`, the way a real
  login outlives the environment.
