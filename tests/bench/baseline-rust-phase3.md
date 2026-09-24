# Phase 3: agentd against bash

`bench/run.py --turns 50`, bash then Rust back to back on a quiet machine
(T3.5), compared with `bench/compare.py`. Rust is the agentd of 35a8437
(checked: a build of that commit is byte-identical): sounds, D-Bus
notifications (to the sink here), seen, background shells and reminders in
the daemon, the tick walking the agents' trees (T3.6), and tmux reached over
a control client (T4.1's transport is already in this build). Only the blink
is still the bash `agent-blink` (phase 4). Targets (design.md): p50 ≤ 5 ms,
p99 ≤ 15 ms, one resident process of ≤ 10 MB, blink ≤ 1% of a core.

- bash: Load average (1 min) 2.2 before, 1.3 after, 16 CPUs.
- rust: Load average (1 min) 1.3 before, 1.3 after, 16 CPUs.

Hook latency, ms (p50 / p99):

| Agent | Event | bash | rust |
|---|---|---|---|
| Claude | (calibration: /bin/true) | 0.5 / 9.0 | 0.5 / 1.0 |
| Claude | SessionStart | 21.5 / 21.5 | 2.2 / 2.2 |
| Claude | UserPromptSubmit | 23.5 / 26.5 | 2.3 / 3.6 |
| Claude | PreToolUse | 23.1 / 27.8 | 1.7 / 12.5 |
| Claude | PostToolUse | 20.3 / 23.6 | 1.6 / 2.2 |
| Claude | SubagentStart | 26.6 / 29.1 | 1.4 / 2.2 |
| Claude | SubagentStop | 21.0 / 23.2 | 1.4 / 2.3 |
| Claude | PermissionRequest | 30.2 / 32.7 | 1.4 / 2.2 |
| Claude | PostToolUse (answer) | 95.4 / 102.0 | 1.3 / 1.7 |
| Claude | Notification | 17.7 / 19.9 | 1.2 / 3.0 |
| Claude | PreCompact | 20.1 / 23.3 | 1.4 / 2.0 |
| Claude | PostCompact | 20.2 / 23.6 | 1.3 / 1.7 |
| Claude | Stop | 70.2 / 75.4 | 1.4 / 1.8 |
| Claude | SessionEnd | 92.1 / 92.1 | 2.9 / 2.9 |
| Codex | SessionStart | 135.9 / 135.9 | 2.4 / 2.4 |
| Codex | UserPromptSubmit | 135.0 / 217.8 | 2.6 / 3.1 |
| Codex | PreToolUse | 136.1 / 225.3 | 1.9 / 12.1 |
| Codex | PermissionRequest | 145.9 / 206.0 | 1.7 / 2.6 |
| Codex | PostToolUse | 204.2 / 276.7 | 1.6 / 2.3 |
| Codex | Stop | 172.2 / 218.0 | 1.7 / 2.5 |
| Codex | SessionEnd | 159.7 / 159.7 | 2.3 / 2.3 |

### bash

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 44.8 | 16.0 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 226.1 | 35.8 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.73 | 0.57 |
| nothing blinking | 0.00 | 0.00 |

### rust

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 2 | 9.4 | 4.0 | agentd ×1, bash ×1 |
| after 20 waits on one pane | 1 | 4.9 | 2.5 | agentd ×1 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.70 | 0.57 |
| nothing blinking | 0.00 | 0.00 |

What it says: every hook, Claude and Codex, now takes 1.2-2.9 ms at p50 and
at most ~3.6 ms at p99, except PreToolUse (12.5 and 12.1 ms: with 50 turns
p99 is the maximum, one sample each; tail.py's 300-turn runs put p99 at
~3 ms). The targets are met. Leaving `needs` (95 → 1.3 ms) and SessionEnd (92 → 2.9 ms) no longer
wait for a Python notification helper. The steady scene keeps two
processes, agentd and the bash blink animator (9.4 MB RSS together), and 20
waits leave agentd alone (4.9 MB). The blink is the same in both, ~9% of a
core: phase 4.
