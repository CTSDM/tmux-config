# Phase 2: agentd against bash, Codex included

`bench/run.py --turns 50`, bash then Rust back to back on a quiet machine
(T2.5), compared with `bench/compare.py`. Rust is the agentd of dc66868:
Claude and Codex (hook, bookkeeping, rollout observation, reconcile) in the
daemon, `@agentd` set so agents.conf and agent-reconcile hand over to it;
sounds, notifications, background shells and the blink still run by the
bash helpers (phase 3 and 4). Targets (design.md): p50 ≤ 5 ms, p99 ≤ 15 ms,
one resident process of ≤ 10 MB, blink ≤ 1% of a core.

- bash: Load average (1 min) 1.5 before, 1.7 after, 16 CPUs.
- rust: Load average (1 min) 1.7 before, 1.0 after, 16 CPUs.

Hook latency, ms (p50 / p99):

| Agent | Event | bash | rust |
|---|---|---|---|
| Claude | (calibration: /bin/true) | 0.6 / 12.6 | 0.6 / 11.2 |
| Claude | SessionStart | 22.5 / 22.5 | 7.7 / 7.7 |
| Claude | UserPromptSubmit | 25.0 / 50.7 | 7.8 / 12.0 |
| Claude | PreToolUse | 25.4 / 30.0 | 6.7 / 11.0 |
| Claude | PostToolUse | 22.1 / 26.8 | 6.5 / 10.6 |
| Claude | SubagentStart | 28.5 / 37.2 | 6.1 / 9.9 |
| Claude | SubagentStop | 22.9 / 27.6 | 6.1 / 10.6 |
| Claude | PermissionRequest | 32.8 / 42.0 | 6.4 / 13.1 |
| Claude | PostToolUse (answer) | 89.0 / 99.7 | 65.0 / 107.5 |
| Claude | Notification | 18.7 / 20.7 | 4.4 / 6.2 |
| Claude | PreCompact | 21.7 / 26.4 | 6.8 / 11.3 |
| Claude | PostCompact | 21.5 / 26.0 | 6.6 / 10.0 |
| Claude | Stop | 75.1 / 96.0 | 6.6 / 11.4 |
| Claude | SessionEnd | 79.6 / 79.6 | 60.5 / 60.5 |
| Codex | SessionStart | 142.0 / 142.0 | 8.1 / 8.1 |
| Codex | UserPromptSubmit | 140.4 / 168.8 | 7.8 / 18.4 |
| Codex | PreToolUse | 142.7 / 234.2 | 6.5 / 20.7 |
| Codex | PermissionRequest | 155.2 / 188.0 | 6.2 / 8.2 |
| Codex | PostToolUse | 206.0 / 226.1 | 63.2 / 72.1 |
| Codex | Stop | 179.8 / 217.1 | 16.9 / 20.9 |
| Codex | SessionEnd | 163.9 / 163.9 | 60.4 / 60.4 |

### bash

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 45.1 | 15.9 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 227.0 | 35.8 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.83 | 0.57 |
| nothing blinking | 0.00 | 0.00 |

### rust

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 4 | 20.5 | 6.0 | agentd ×1, bash ×2, sleep ×1 |
| after 20 waits on one pane | 1 | 4.4 | 2.0 | agentd ×1 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.77 | 0.57 |
| nothing blinking | 0.00 | 0.00 |

What it says: Codex events went from 140-200 ms (a Python start per call) to
6-8 ms at p50; Stop takes ~17 ms, as it recounts the background command
trees over /proc. The ~60 ms left on Codex PostToolUse (here, the answer that
leaves `needs`), SessionEnd and Claude's leaving `needs` is the synchronous
`agent-notify --close` of the bash helper, until phase 3. The Codex observer
now lives in the daemon (bash's resident `python3` watcher is gone).

## The p99 tail (300 turns)

`bench/tail.py --turns 300` (agentd of 9b563e5, the T3.1 build; the hook
and observation paths are those of phase 2), load ~1.5, run twice. Each
scene is 300 × (UserPromptSubmit, PreToolUse), 5-50 ms apart at random so
nothing keeps a fixed phase against a 2 s tick. A thread polls the daemon
every ~0.2 ms: the children it has (the `tmux` it spawns) and whether one
of its threads is running (state R). "Background" is what began while no
hook was running.

Second run:

| Scene | Event | n | p50 | p99 | max | > 15 ms | of them, daemon busy at start | of them, background child during | of them, background CPU during | all hooks, daemon busy at start |
|---|---|---|---|---|---|---|---|---|---|---|
| claude alone | UserPromptSubmit | 300 | 7.5 | 13.1 | 20.4 | 2 | 0 | 0 | 0 | 0.0% |
| claude alone | PreToolUse | 300 | 7.5 | 12.9 | 14.0 | 0 | 0 | 0 | 0 | 0.0% |
| codex | UserPromptSubmit | 300 | 7.2 | 9.5 | 18.2 | 3 | 0 | 0 | 2 | 0.3% |
| codex | PreToolUse | 300 | 7.3 | 9.6 | 17.2 | 2 | 0 | 0 | 0 | 0.0% |
| claude + codex watched | UserPromptSubmit | 300 | 7.1 | 15.1 | 23.4 | 4 | 0 | 0 | 1 | 0.0% |
| claude + codex watched | PreToolUse | 300 | 7.2 | 15.2 | 25.2 | 4 | 0 | 0 | 0 | 0.3% |


Background work (the daemon started a child while no hook ran), and when the slow hooks came:

- claude alone, UserPromptSubmit over 15 ms at turn: #86 20, #249 16
- claude alone: 1 background busy intervals in 21 s, median 4.6 ms
- claude alone: no background CPU bursts over 2 ms
- codex, UserPromptSubmit over 15 ms at turn: #31 18, #170 17, #225 16
- codex, PreToolUse over 15 ms at turn: #111 17, #196 16
- codex: 12 background busy intervals in 21 s, median 2.7 ms
- codex: 8 background CPU bursts over 2 ms, median 10.2 ms, max 10.7 ms, every 2.01 s (median gap)
- claude + codex watched, UserPromptSubmit over 15 ms at turn: #31 16, #177 15, #230 15, #232 23
- claude + codex watched, PreToolUse over 15 ms at turn: #14 25, #148 15, #204 19, #210 16
- claude + codex watched: 8 background busy intervals in 21 s, median 2.7 ms
- claude + codex watched: 6 background CPU bursts over 2 ms, median 9.9 ms, max 17.9 ms, every 2.01 s (median gap)

First run, the same scenes: slow hooks 0 / 0 (Claude alone), 4 / 2 (Codex),
3 / 0 (Claude with a Codex pane watched); background CPU bursts only with a
Codex observation, ~10 ms every 2.01 s.

What the daemon does in those bursts, from `/proc/<agentd>/io` over 10 s
with no hooks at all:

| Scene | Read syscalls per 2 s | Read per 2 s | CPU per 2 s |
|---|---|---|---|
| Claude only (no observation) | 0 | 0 KiB | 0 ms |
| a Codex turn open | ~3,700 (~5.4 per process; 692 on the machine) | ~186 KiB | ~10 ms |

**Reading:** there is a real tail, and part of it is the observation tick.
While a Codex turn (or its commands) is observed, the daemon scans all of
`/proc` every 2 s (X7's command trees), ~10 ms of CPU on its only thread;
a hook that arrives then waits up to that long. That is ~1% of hooks at this
pace (a 7 ms hook meets a 10 ms burst every 2 s), and it matches what the
tables show: in the Codex scenes at least a third of the hooks over 15 ms
overlap a burst (7 of 22 over both runs; a lower bound, since a burst that
begins while a hook waits on its `tmux` call is not counted as background),
and p99 sits around 10-16 ms.
The rest of the slow hooks are ordinary scheduling noise: Claude alone,
with no timer at all, also has 0-2 per 300, at 16-20 ms, and the
`/bin/true` calibration shows the same kind of outliers. The burst scales
with the number of processes on the machine, not with the agents.
