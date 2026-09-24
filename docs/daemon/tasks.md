# agentd: tasks

Owner: **arquitecto** (only writer). Status: `todo`, `doing`, `review`, `done`.
Report progress by message with the task id; the architect updates this file.

## Phase 0: preparation — done

| Id | Owner | Status | Task |
|---|---|---|---|
| T0.1 | arquitecto | done | design.md, contract.md, tasks.md |
| T0.2 | implementador | done (1f443c1) | Bash test seams (design.md, "Test seams"): `AG_SINK` in `agent-sound` and `agent-notify` (both `show` and `--close`), `AG_FOCUS_CLIENT` in `ag_focused_client`. No other behavior change. |
| T0.3 | implementador | done (spike-control-mode.md) | Spike, control mode on tmux 3.6, findings in `docs/daemon/spike-control-mode.md` (send them to the architect, who commits them). Questions below. |
| T0.4 | implementador | done (b1c8aca) | Crate skeleton in `agentd/`: `Cargo.toml` (edition 2024, release profile with `lto`, `codegen-units = 1`, `panic = "abort"`, `strip`), `deny.toml` (crates.io only, license allowlist), `agentd/check.sh` (fmt, clippy -D warnings, test, deny), `target/` ignored, subcommand stubs. Dependencies of design.md only. |
| T0.5 | tester | done (204874f) | Harness in `tests/` (uv + pytest + pyright strict): isolated tmux server fixture, fake agent processes, a way to run the hook as a child of the fake agent, sink reader, option snapshots. Selectable implementation: `AGENT_IMPL=bash` (the worktree's `agents/bin/agent-hook`) or `AGENT_IMPL=rust` (path in `AGENTD`). |
| T0.6 | tester | done (6c68ef8) | Contract suite on bash, section by section as contract.md lands. Intentional changes (`CHANGE` in the contract) are expected failures on bash. Needs T0.2 for sounds, notifications and visibility. |
| T0.7 | tester | done (6c68ef8, bench/baseline-bash.md) | Benchmarks in `tests/bench/`: hook latency per event (p50/p90/p99), RSS of resident processes, CPU of the blink over 30 s. Baseline numbers on bash into the report. |

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

## Phase 1: core, hook, daemon (Claude) — done

Starts while T0.6 finishes: the suite grows as the code does. Goal: the whole
Claude part of the contract suite green with `AGENT_IMPL=rust`, effects still
through the bash helpers (design.md, "Migration"). Known exceptions until
phase 3: O3 / C5 (a spawned `agent-notify` publishes its id ~150 ms late,
like bash) and C6 (`agentd ctl reconcile` delegates to bash `agent-reconcile`
in phase 1). Also until phase 3: an event that closes a bash notification
acks after `agent-notify --close` has run (~150-300 ms, uv), as bash does;
and C3 (the bash helpers write `@agent_notify_id`, `@agent_notify_pid`,
`@agent_bg_watch`).

| Id | Owner | Status | Task |
|---|---|---|---|
| T1.1 | implementador | done (2e7e6d5) | **Core, pure** (`agentd/src/core/`): inputs (hook event with the I3 fields, pane snapshot per P2 with the facts V2 needs, global config of §13, now), effects (option writes, sound, notify, notify-close, reminder arm/cancel, start bgwatch, start blink), and the rules H1-H14, A1-A3, R, S1-S6 (which sound, gated by visibility and mute), N1-N3 (which notification, which close), N4 with C1 and C2, O1/O3 in the order of the effect list. Unit tests named after rule ids. No I/O, no clock. |
| T1.2 | implementador | done (2ac5eb2) | **Hook client** (`agentd hook`): I1-I3 parsing with jq `//` semantics, whitespace collapse, 300 characters; parent chain from `/proc` (pid, comm, start time) for I4 (up to 16 entries or pid 1); the protocol of design.md; `ensure` and retry for 300 ms when there is no daemon; never stdout, always 0. Measure p50/p99. |
| T1.3 | implementador | done (fe2e20b, 9f7670f) | **Daemon** (`agentd daemon`, `ensure`, `ctl status`): socket per server (identity.rs), flock, pidfd on the tmux server; per event one tmux read (pane pid, the P2 options, session, window/pane active, title, space, mute, §13 options) and one write before the ack; I4 with the chain (C4); visibility V1-V2 in-process (Hyprland `.socket.sock` `j/activewindow`, `AG_FOCUS_CLIENT`, skip control-mode clients); effects by spawning the helpers of `@agents_bin` (`agent-sound`, `agent-notify`, `agent-bgwatch`, `agent-blink`), except reminders: timers in the daemon, one per pane (C1, C2), firing spawns sound + notify; rounds and subagents in memory, persisted in the state file and read on start. `ctl status`: JSON of the daemon's state, for tests and debugging. |
| T1.5 | implementador | done (bf79b33) | U1 (the user asked for it): in theme.conf's `@fleet-chips-tpl`, wrap each chip, current and others, in `#[range=session\|#{session_id}]`…`#[norange]`; check it in an isolated server with the full tmux.conf (click, narrow layout with several chip rows, `_peek-*` still hidden). Small: do it when it fits between T1.2 and T1.3. |
| T1.6 | tester | done (01bed84) | U1 test: real client with `mouse on`, theme.conf and `agent-spaces load` sourced, an SGR mouse click (`\e[<0;X;Ym` down/up) on a chip of the top row → `client_session` changes; a chip of another space can't be clicked because it isn't there. C8: xfail strict on bash until T1.5 lands. |
| T1.7 | implementador | done (d397371) | Long life (review of T1.3): (a) drop a pane's event and effect queues, and its reminder, once they are drained and the pane is gone (tmux read finds no pane) or after its SessionEnd; today a daemon that runs for weeks keeps a task and a channel per pane ever seen. (b) The state file belongs to one tmux server instance: store the server's pid and start time and ignore a file from another one (a restarted server reuses the socket path, so the id, and pane ids restart at `%0`: old rounds and reminders would land on new panes); delete it when the daemon exits because the server is gone. Tests for both. |
| T1.4 | tester | done (cebc2a5, 82b7614, bench/baseline-rust-phase1.md) | Run the suite with `AGENT_IMPL=rust` on each implementer delivery; report failures by rule id to both. Latency and RSS of `agentd` with T0.7's benchmarks. |

## Phase 2: Codex, and reconcile in the daemon — done

Goal: the whole contract suite green with `AGENT_IMPL=rust`, Codex included
(§6), except O3/C5 until phase 3. Reconcile moves here, not in phase 3: the
bash `agent-reconcile` handles Codex panes by running the bash hook, which
would keep its own Codex bookkeeping on panes the daemon owns.

| Id | Owner | Status | Task |
|---|---|---|---|
| T2.1 | implementador | done (573a87e) | **Codex core, pure:** X1-X4 and X6 (session and turn, calls and waits by fingerprint, unresolved and unmatched permissions, the oldest wait shows, question sent, the Codex options), X3 states including `Interrupt` and compaction, and the shared effects (§8, §11, N4) for Codex as agent-hook does after `agent-codex prepare`. Reference: `transition()` and `prepare()` in `agents/bin/agent-codex`. Unit tests by rule id, including the pure scenarios of `agents/tests/test_codex.py`. |
| T2.2 | implementador | done (24d11d4) | **Hook client for Codex:** also `turn_id`, `tool_response.accepted` (object or JSON string), and the fingerprint of `[tool, tool_input without "description"]` (SHA-256 of JSON with sorted keys and no spaces), computed in the client so no tool input reaches the socket (X8). |
| T2.3 | implementador | done (3b42a28) | **Observation (X5, X7) in the daemon:** one shared 2 s tick that runs only while some pane is watched (no timer otherwise), one `/proc` scan per tick for all watched panes (command roots, X7), the rollout read incrementally per pane (device and inode, offset, never an unfinished line, restart on truncation or another file), synthesized events through the pane's event queue (O2), X5's end conditions, agent gone → as H11 (notification closed). Persist only ids, offsets, fingerprints and process identities (X8): no messages, prompts or commands in the state file. |
| T2.4 | implementador | done (c8cb42f) | **Reconcile in the daemon** (E2, B2, C6): `agentd ctl reconcile [pane...]` is a daemon request, no longer bash. Agent gone → clear and close its notification; Claude busy with a transcript that says the turn is over → idle (last 80 lines); background shells without a watcher → start `agent-bgwatch` (still bash); Codex → one observation now, and resume watching. |
| T2.6 | implementador | done (dc66868) | **The `@agentd` switch** (design.md), delivered with T2.4 and never before it: `agents.conf` runs `#{@agentd} ensure` on load when set; `agent-reconcile` starts with "if `@agentd` is set, exec `agentd ctl reconcile "$@"`". Nothing else changes yet. |
| T2.7 | tester | done (f9b9407) | With `AGENT_IMPL=rust`, set `@agentd` (= `AGENTD`) globally before sourcing agents.conf, so tmux hooks, the board and load-time reconcile reach the daemon; keep the harness's own `ensure` only for servers without agents.conf. Lands after T2.4 + T2.6. |
| T2.5 | tester | done (f9b9407, bench/baseline-rust-phase2.md) | Suite with `AGENT_IMPL=rust` on each delivery (§6, E2 and C6 now expected green), and Codex latency with bench/run.py next to bash. |

## Phase 3: effects in-process — done

Goal: the whole suite green with `AGENT_IMPL=rust` and `@agentd` set, no
exception left (O3/C5, C3, C6), and no bash helper started by the daemon
except `agent-blink` (phase 4) and `agent-jump` (a click). The bash scripts
stay for bash mode.

| Id | Owner | Status | Task |
|---|---|---|---|
| T3.1 | implementador | done (9b563e5) | **Sounds (S9):** file lookup (`AG_SOUNDS`, `$XDG_DATA_HOME/tmux-agents/sounds`, wav/ogg/oga/mp3/flac), `@agent_sound` and `@agent_sound_volume`, player as agent-sound (`AG_SOUND_PLAYER`, `pw-play --volume`, ffplay, mpv, aplay), priority and the 2.5 s debounce shared with bash and other daemons through `$XDG_RUNTIME_DIR/tmux-agents/sound.last` under the same `flock` (one debounce per user, S9), a higher priority stops the previous player. Sink line at the same point as bash. |
| T3.2 | implementador | done (3ab2561) | **Notifications over D-Bus (zbus):** one session connection and one signal match; `Notify` as agent-notify (app `tmux agents`, icon `utilities-terminal`, actions `default`/`Open`, urgency hint, `replaces_id` = the pane's open one, timeout -1); `ActionInvoked` default → `agent-jump <pane>`; `NotificationClosed` → forget. Closes per N3, in effect order (O3). Ids per pane in memory and in the state file, so a restarted daemon can still close them. No pane options (C3). No bus → nothing (debug log). Sink seam. |
| T3.3 | implementador | done (3ab2561) | **Seen:** `agentd ctl seen <pane>` (E1: `done` → `idle` without touching `@agent_since`, close its notification). agents.conf: with `@agentd`, `pane-focus-in` runs it for panes with `@agent` instead of hooks [40]/[41]; without `@agentd`, as today. |
| T3.4 | implementador | done (35a8437) | **Background shells (B1, B2) in the daemon:** Claude panes with such shells join the shared tick and its `/proc` scan; `@agent_bg` kept there; no `agent-bgwatch`, no `@agent_bg_watch` (C3). |
| T3.6 | implementador | done (35a8437) | **The tick must not scan all of /proc** (tester's tail analysis, bench/baseline-rust-phase2.md "The p99 tail": ~10 ms of CPU and ~3,700 reads per tick on the runtime thread while Codex is observed, which delays ~1% of hooks by up to 10 ms). X7 and B1 walk the agent's descendants through `/proc/<pid>/task/*/children` (every thread), plus the kept `(pid, start)` of known groups and their descendants, instead of every process: the same trees, at a cost that follows the agent's tree, not the machine. Only when `children` files don't exist (kernel without CONFIG_PROC_CHILDREN), the full scan, and then off the runtime thread (`spawn_blocking`). Check with `bench/tail.py`: no burst per tick left. |
| T3.5 | tester | done (6e77864, de4a2d6, bench/baseline-rust-phase3.md) | Suite and bench on each delivery; at the end of the phase, no expected failure left in Rust. Latency of leaving `needs` and of SessionEnd next to bash. |

## Phase 4: control mode and the blink in the daemon

Goal: hook p50 ≤ 5 ms and the blink ≤ 1% of a core for `agentd`, with the
daemon invisible (contract §16, Z1). Recipe and pitfalls:
[spike-control-mode.md](spike-control-mode.md), case k.

| Id | Owner | Status | Task |
|---|---|---|---|
| T4.1 | implementador | done (3cd103d) | **Control-mode transport** behind the `Tmux` trait: `tmux -C new-session -A -s _peek-agentd <inert>`, then `destroy-unattached on`, flags `no-output,ignore-size`, size `1000x100`; answers matched to commands by number (`%begin`/`%end`/`%error`), never by content (output is not escaped); formats without `%` (control mode's `display -p` expands strftime); reattach after `%exit` while the server lives, EOF = server gone. If the control client can't attach, the spawn transport, with a debug log line. |
| T4.2 | implementador | done (76f20a1) | **Skip control clients in bash** (`#{client_control_mode}`): `agent-spaces layout` (per-session client width), `agent-jump` (viewer and most recently active client), `ag_focused_client`'s tmux-flag fallback. The session name alone is not the protection. |
| T4.3 | implementador | done (19b8092) | **Blink in the daemon** (K1-K5): same targets, text, frames and timing; writes only the options that changed from the previous frame; `agentd ctl blink-demo`; with `@agentd`, agents.conf's load-time start and the prefix+Q preview (`agent-spaces` menu) go to the daemon, which no longer starts `agent-blink`. |
| T4.5 | implementador | done (2255a79) | **Z1 gaps** (tester's findings): (a) on `%sessions-changed`, if the daemon's session is the only one left, close it and stop reattaching, so the server exits as without the daemon (today it keeps the server alive); (b) agents.conf rebinds `prefix s`, `w` (`choose-tree -Zs`/`-Zw -f '#{?#{m:_peek-*,#{session_name}},0,1}'`) and `D` (`choose-client -Z -f '#{==:#{client_control_mode},0}'`), in both modes; (c) a plain `tmux attach` must pick a user session: check what tmux 3.6 picks while the daemon's session is the most recent one, and fix it if it can be the daemon's. |
| T4.4 | tester | doing | Tests for Z1 with `AGENT_IMPL=rust` (attach, run, kill its session, last user session closed, daemon exit: sizes, layout options, seen, `agent-jump`'s pick, lists, `session_attached`); K with the in-process blink; bench: hook latency and blink CPU of `agentd` next to phase 3. |

Subscriptions (`refresh-client -B`) are left out: with control mode the
blink's refresh (`list-panes -a` every 6 frames) is cheap. Revisit if a
measurement shows polling.

## Phase 5: cutover (with the user)

Everything up to the last step happens off the live system. The last step,
switching the live setup, is done with the user, who decides when.

| Id | Owner | Status | Task |
|---|---|---|---|
| T5.1 | implementador | todo | **`agent-hook` follows the switch** (design.md, "Agents already running at cutover"): after I1, if `@agentd` is set and executable, `exec "$agentd" hook "$kind"` with the same stdin. Bash Codex observers of before (`--observe`) then end up in `agentd hook`, where I4 drops them (not the agent's descendants): check it. `pane-focus-in[41]` goes back to closing `@agent_notify_id` in both modes (only bash sets it, C3), so notifications shown before cutover still close when seen. |
| T5.2 | implementador | todo | **Rollback switch for `agentd hook`:** if `$XDG_RUNTIME_DIR/tmux-agents/agentd.off` exists, the hook client runs the bash `agent-hook <kind>` next to it (`@agents_bin` is not needed: the path comes from the same install) with the payload it read, waits, exits 0; and `agentd daemon`/`ensure` exit at once. One `stat` per event. |
| T5.3 | implementador | todo | **Install:** `setup.sh` builds `agentd` (`cargo build --release --locked`) and installs it atomically to `~/.local/bin/agentd` (temp file + rename, so a running daemon keeps its inode); `agentd ctl stop` (the daemon saves its state, clears its blink options and exits; the next hook starts the new one). `agents/install --agentd <path>` writes `<path> hook claude\|codex` into every Claude profile and Codex (same timeouts, backups, `--print`), `agents/install --bash` writes `agent-hook` back. `tmux.conf`: `if-shell 'test -x "$HOME/.local/bin/agentd"'` → `set -g @agentd` before `agents.conf` is sourced, so a machine without the build stays on bash. |
| T5.4 | implementador | todo | **Docs:** README, docs/guide.md (what the user sees: nothing changes, `agentd ctl status`, how to go back), docs/internals.md (agentd: shape, switch, transport, observation, state file, debugging with `AGENTD_TRANSPORT=spawn` and the debug log). |
| T5.5 | tester | todo | **Rehearsal** on an isolated server with the daemon branch's full `tmux.conf`: fake agents started with the bash hook, then the switch (install `--agentd`, `@agentd` set): old agents keep working through `agent-hook` → `agentd hook` (one owner: no bash Codex bookkeeping, no second animator, pre-cutover notifications close when seen); new agents use `agentd hook`. Then the rollback (`agentd.off`, `--bash`, `@agentd` unset, `ctl stop`): back to bash with the same checks. Real `setup.sh` into a temp `HOME`. |
| T5.6 | arquitecto + user | todo | **The switch, live:** merge `daemon` into `main`, `setup.sh`, `agents/install --agentd ~/.local/bin/agentd`, reload tmux, check with `agentd ctl status` and real sessions; rollback steps ready. Only when the user says so. |

Phases 6 (spaces, optional) and 7 (deleting bash, after a while live) later.
