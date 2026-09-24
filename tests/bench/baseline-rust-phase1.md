# Phase 1: agentd against bash

`bench/run.py --turns 50`, bash then Rust back to back on the same machine
state (T1.4). Rust is phase 1: Claude only (Codex events are ignored, so
its Codex rows only time the no-op path), with sounds, notifications,
background shells and the blink still run by the bash helpers.

**Conditions:** the machine was busy with other work (load average ~22 on
16 CPUs) for both runs; the calibration rows show the same state. Absolute
numbers are inflated; compare the columns. [baseline-bash.md](baseline-bash.md)
has bash on a quiet machine.

- bash: Load average (1 min) 21.4 before, 23.0 after, 16 CPUs.
- rust: Load average (1 min) 23.0 before, 22.7 after, 16 CPUs.

Hook latency, ms (p50 / p99):

| Agent | Event | bash | rust |
|---|---|---|---|
| Claude | (calibration: /bin/true) | 3.5 / 24.1 | 3.5 / 22.7 |
| Claude | SessionStart | 66.2 / 66.2 | 26.3 / 26.3 |
| Claude | UserPromptSubmit | 69.3 / 163.9 | 23.1 / 41.8 |
| Claude | PreToolUse | 71.6 / 159.6 | 22.6 / 61.4 |
| Claude | PostToolUse | 58.7 / 107.6 | 24.0 / 56.1 |
| Claude | SubagentStart | 82.8 / 148.6 | 22.4 / 204.1 |
| Claude | SubagentStop | 62.4 / 105.0 | 22.2 / 39.7 |
| Claude | PermissionRequest | 97.1 / 212.5 | 22.6 / 28.5 |
| Claude | PostToolUse (answer) | 222.5 / 343.3 | 163.5 / 244.2 |
| Claude | Notification | 50.6 / 102.4 | 15.0 / 23.7 |
| Claude | PreCompact | 59.5 / 125.6 | 23.2 / 64.6 |
| Claude | PostCompact | 61.1 / 86.9 | 23.7 / 36.5 |
| Claude | Stop | 184.8 / 428.7 | 24.2 / 67.0 |
| Claude | SessionEnd | 198.0 / 198.0 | 158.6 / 158.6 |
| Codex | SessionStart | 451.4 / 451.4 | 15.8 / 15.8 |
| Codex | UserPromptSubmit | 387.5 / 641.7 | 15.0 / 21.1 |
| Codex | PreToolUse | 383.9 / 842.1 | 15.3 / 25.3 |
| Codex | PermissionRequest | 417.0 / 678.8 | 15.4 / 42.7 |
| Codex | PostToolUse | 508.3 / 637.2 | 14.7 / 94.5 |
| Codex | Stop | 468.8 / 723.0 | 15.3 / 22.0 |
| Codex | SessionEnd | 386.9 / 386.9 | 21.9 / 21.9 |

### bash

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 44.7 | 15.6 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 225.5 | 35.5 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 16.33 | 0.90 |
| nothing blinking | 0.00 | 0.00 |

### rust

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 4 | 20.1 | 5.5 | agentd ×1, bash ×2, sleep ×1 |
| after 20 waits on one pane | 1 | 4.2 | 1.9 | agentd ×1 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 15.87 | 0.83 |
| nothing blinking | 0.00 | 0.00 |
