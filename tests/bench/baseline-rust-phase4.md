# Phase 4: agentd against bash

`bench/run.py --turns 50`, bash then Rust back to back on a quiet machine
(T4.4), compared with `bench/compare.py`. Rust is agentd built here from
4bcc9d5 (T4.3): everything in the daemon, the blink included, over its
control-mode client (T4.5 changes Z1 only). CPU comes from schedstat (ns)
plus reaped children: the daemon's animator stays below one clock tick per
second. Targets (design.md): p50 ≤ 5 ms, p99 ≤ 15 ms, one resident process
of ≤ 10 MB, blink ≤ 1% of a core, 0 idle.

- bash: Load average (1 min) 3.0 before, 1.6 after, 16 CPUs.
- rust: Load average (1 min) 1.6 before, 0.9 after, 16 CPUs.

Hook latency, ms (p50 / p99):

| Agent | Event | bash | rust |
|---|---|---|---|
| Claude | (calibration: /bin/true) | 0.6 / 40.5 | 0.6 / 14.3 |
| Claude | SessionStart | 21.9 / 21.9 | 1.8 / 1.8 |
| Claude | UserPromptSubmit | 23.6 / 26.8 | 2.5 / 3.7 |
| Claude | PreToolUse | 23.2 / 27.0 | 1.8 / 2.7 |
| Claude | PostToolUse | 20.3 / 22.9 | 1.6 / 2.4 |
| Claude | SubagentStart | 26.5 / 39.8 | 1.4 / 2.3 |
| Claude | SubagentStop | 20.7 / 27.8 | 1.5 / 2.3 |
| Claude | PermissionRequest | 30.5 / 34.7 | 1.5 / 1.9 |
| Claude | PostToolUse (answer) | 94.8 / 100.9 | 1.4 / 1.8 |
| Claude | Notification | 18.0 / 19.8 | 1.4 / 1.9 |
| Claude | PreCompact | 20.2 / 27.0 | 1.5 / 3.3 |
| Claude | PostCompact | 20.0 / 23.3 | 1.4 / 2.1 |
| Claude | Stop | 69.5 / 75.1 | 1.6 / 3.1 |
| Claude | SessionEnd | 90.0 / 90.0 | 2.0 / 2.0 |
| Codex | SessionStart | 152.1 / 152.1 | 1.8 / 1.8 |
| Codex | UserPromptSubmit | 133.1 / 163.3 | 2.6 / 3.5 |
| Codex | PreToolUse | 135.6 / 228.7 | 1.9 / 2.6 |
| Codex | PermissionRequest | 145.7 / 167.7 | 1.7 / 12.1 |
| Codex | PostToolUse | 205.8 / 222.9 | 1.7 / 2.4 |
| Codex | Stop | 172.5 / 196.5 | 1.7 / 2.6 |
| Codex | SessionEnd | 170.3 / 170.3 | 2.4 / 2.4 |

### bash

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 45.1 | 16.1 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 225.2 | 35.7 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.96 | 0.54 |
| only unseen blinking | 4.67 | 0.28 |
| nothing blinking | 0.00 | 0.00 |

### rust

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 1 | 5.1 | 2.7 | agentd ×1 |
| after 20 waits on one pane | 1 | 4.9 | 2.6 | agentd ×1 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 0.13 | 0.20 |
| only unseen blinking | 0.07 | 0.10 |
| nothing blinking | 0.00 | 0.00 |

What it says: every target is met. The blink costs 0.13% of a core in
agentd and 0.20% in the tmux server while a pane needs you (bash 8.96% +
0.54%), about half for the soft `unseen` blink, and nothing at all without
targets. agentd is the only resident process, ~5 MB RSS, whatever happened
before. Hooks take 1.5-2.6 ms at p50.
