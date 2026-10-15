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
window) has `_slow_` right before the multiplexer, `d5_…_slow_tmux`:
`e2e/run.sh _slow_` runs only those, `--skip _slow_` leaves them out.

## Failures

A failing scenario keeps its sandbox in `target/e2e/artifacts/<test name>/` and
prints the path, the last lines of the Claude pane and of the claude-nonstop
log. Inside:

| path | what |
|---|---|
| `screens/*.txt` | pane scrollback at the moment of failure |
| `screens/*.feed` | raw pane output (tmux `pipe-pane`, `zellij subscribe`) |
| `claude-debug.log` | Claude Code `--debug-file` |
| `mock/requests.jsonl` | every API request with `last_user_text` and the reply it got |
| `hooks/` | StopFailure, Stop and SessionStart payloads in order |
| `calls/` | fake `notify-send`, `curl`, `systemctl` invocations |
| `home/.claude/settings.json`, `home/.claude/projects/` | settings and transcripts |
| `home/.local/state/claude-nonstop/` | `ledger.json`, `cutoff/`, `log/claude-nonstop.log` |

A message that starts with `KNOWN BUG <n>` or `PRODUCT BUG` is the product
misbehaving as documented, not the harness. `timed out after Ns waiting for X`
names the event that never happened; the artifacts show how far it got.

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
- `claude-nonstop` runs with `LIMIT_MARGIN_SECONDS=0`, `LIMIT_VERIFY_SECONDS=1`,
  `OVERLOAD_BACKOFF_SECONDS=1,2`, `OVERLOAD_JITTER_PERCENT=0`; anything else goes
  through `.env(..)`. An unknown `CLAUDE_NONSTOP_*` variable breaks every command.
- Claude Code does not pass `CLAUDE_CODE_OAUTH_TOKEN` to its hooks, so a
  subscription Claude Code that claude-nonstop starts from a hook's resumer (a
  reopened session, a queued prompt) is not logged in. `.login_in_settings()`
  also stores the credentials in the `env` of `settings.json`, the way a real
  login outlives the environment.
- Scenarios that wait out claude-nonstop's fixed delays (the 60 s retry backoff
  and longer) have `slow` right after the matrix id, `f3_slow_…`:
  `e2e/run.sh --skip slow` leaves them out, `e2e/run.sh slow` runs only them.
