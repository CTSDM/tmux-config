# agentd: tasks

Owner: **arquitecto** (only writer). Status: `todo`, `doing`, `review`, `done`.
Report progress by message with the task id; the architect updates this file.

## Phase 0: preparation

| Id | Owner | Status | Task |
|---|---|---|---|
| T0.1 | arquitecto | doing | design.md, contract.md, tasks.md |
| T0.2 | implementador | todo | Bash test seams (design.md, "Test seams"): `AG_SINK` in `agent-sound` and `agent-notify` (both `show` and `--close`), `AG_FOCUS_CLIENT` in `ag_focused_client`. No other behavior change. |
| T0.3 | implementador | todo | Spike, control mode on tmux 3.6, findings in `docs/daemon/spike-control-mode.md` (send them to the architect, who commits them). Questions below. |
| T0.4 | implementador | todo | Crate skeleton in `agentd/`: `Cargo.toml` (edition 2024, release profile with `lto`, `codegen-units = 1`, `panic = "abort"`, `strip`), `deny.toml` (crates.io only, license allowlist), `agentd/check.sh` (fmt, clippy -D warnings, test, deny), `target/` ignored, subcommand stubs. Dependencies of design.md only. |
| T0.5 | tester | todo | Harness in `tests/` (uv + pytest + pyright strict): isolated tmux server fixture, fake agent processes, a way to run the hook as a child of the fake agent, sink reader, option snapshots. Selectable implementation: `AGENT_IMPL=bash` (the worktree's `agents/bin/agent-hook`) or `AGENT_IMPL=rust` (path in `AGENTD`). |
| T0.6 | tester | todo | Contract suite on bash, section by section as contract.md lands. Intentional changes (`CHANGE` in the contract) are expected failures on bash. Needs T0.2 for sounds, notifications and visibility. |
| T0.7 | tester | todo | Benchmarks in `tests/bench/`: hook latency per event (p50/p90/p99), RSS of resident processes, CPU of the blink over 30 s. Baseline numbers on bash into the report. |

### T0.3 questions (control mode)

1. `tmux -C attach -f no-output,ignore-size` (or better flags): does the
   control client change any window size, `session_attached`,
   `#{session_many_attached}`, or fire `client-attached`,
   `client-session-changed`, `client-resized` hooks?
2. Which session must it attach to, and what happens when that session is
   killed, renamed, or is the last one? Can it live without a session?
3. How does it show up in `list-clients` (flags, `client_control_mode`)? List
   every consumer in `agents/` that would need to skip it (e.g. `agent-jump`
   picks the most recently active client).
4. Latency of `set -p` through control mode vs spawning `tmux`, and CPU of
   14 option writes per second both ways.
5. Is `refresh-client -B` (format subscriptions) useful to learn session
   and pane changes without polling? What does it cost?

## Phase 1 onwards

Written when phase 0 closes.
