# agentd: tasks

Owner: **arquitecto** (only writer). Status: `todo`, `doing`, `review`, `done`.
Report progress by message with the task id; the architect updates this file.

## Phase 0: preparation

| Id | Owner | Status | Task |
|---|---|---|---|
| T0.1 | arquitecto | done | design.md, contract.md, tasks.md |
| T0.2 | implementador | done (1f443c1) | Bash test seams (design.md, "Test seams"): `AG_SINK` in `agent-sound` and `agent-notify` (both `show` and `--close`), `AG_FOCUS_CLIENT` in `ag_focused_client`. No other behavior change. |
| T0.3 | implementador | done (spike-control-mode.md) | Spike, control mode on tmux 3.6, findings in `docs/daemon/spike-control-mode.md` (send them to the architect, who commits them). Questions below. |
| T0.4 | implementador | done (b1c8aca) | Crate skeleton in `agentd/`: `Cargo.toml` (edition 2024, release profile with `lto`, `codegen-units = 1`, `panic = "abort"`, `strip`), `deny.toml` (crates.io only, license allowlist), `agentd/check.sh` (fmt, clippy -D warnings, test, deny), `target/` ignored, subcommand stubs. Dependencies of design.md only. |
| T0.5 | tester | done (204874f) | Harness in `tests/` (uv + pytest + pyright strict): isolated tmux server fixture, fake agent processes, a way to run the hook as a child of the fake agent, sink reader, option snapshots. Selectable implementation: `AGENT_IMPL=bash` (the worktree's `agents/bin/agent-hook`) or `AGENT_IMPL=rust` (path in `AGENTD`). |
| T0.6 | tester | doing | Contract suite on bash, section by section as contract.md lands. Intentional changes (`CHANGE` in the contract) are expected failures on bash. Needs T0.2 for sounds, notifications and visibility. |
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

## Phase 1: core, hook, daemon (Claude)

Starts while T0.6 finishes: the suite grows as the code does. Goal: the whole
Claude part of the contract suite green with `AGENT_IMPL=rust`, effects still
through the bash helpers (design.md, "Migration"). Known exceptions until
phase 3: O3 / C5 (a spawned `agent-notify` publishes its id ~150 ms late,
like bash) and C6 (`agentd ctl reconcile` delegates to bash `agent-reconcile`
in phase 1).

| Id | Owner | Status | Task |
|---|---|---|---|
| T1.1 | implementador | done (2e7e6d5) | **Core, pure** (`agentd/src/core/`): inputs (hook event with the I3 fields, pane snapshot per P2 with the facts V2 needs, global config of §13, now), effects (option writes, sound, notify, notify-close, reminder arm/cancel, start bgwatch, start blink), and the rules H1-H14, A1-A3, R, S1-S6 (which sound, gated by visibility and mute), N1-N3 (which notification, which close), N4 with C1 and C2, O1/O3 in the order of the effect list. Unit tests named after rule ids. No I/O, no clock. |
| T1.2 | implementador | doing | **Hook client** (`agentd hook`): I1-I3 parsing with jq `//` semantics, whitespace collapse, 300 characters; parent chain from `/proc` (pid, comm, start time) for I4 (up to 16 entries or pid 1); the protocol of design.md; `ensure` and retry for 300 ms when there is no daemon; never stdout, always 0. Measure p50/p99. |
| T1.3 | implementador | todo | **Daemon** (`agentd daemon`, `ensure`, `ctl status`): socket per server (identity.rs), flock, pidfd on the tmux server; per event one tmux read (pane pid, the P2 options, session, window/pane active, title, space, mute, §13 options) and one write before the ack; I4 with the chain (C4); visibility V1-V2 in-process (Hyprland `.socket.sock` `j/activewindow`, `AG_FOCUS_CLIENT`, skip control-mode clients); effects by spawning the helpers of `@agents_bin` (`agent-sound`, `agent-notify`, `agent-bgwatch`, `agent-blink`), except reminders: timers in the daemon, one per pane (C1, C2), firing spawns sound + notify; rounds and subagents in memory, persisted in the state file and read on start. `ctl status`: JSON of the daemon's state, for tests and debugging. |
| T1.4 | tester | todo | Run the suite with `AGENT_IMPL=rust` on each implementer delivery; report failures by rule id to both. Latency and RSS of `agentd` with T0.7's benchmarks. |

Phases 2-5: written when phase 1 closes.
