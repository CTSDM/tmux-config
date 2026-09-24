# Phase 1: agentd against bash

`bench/run.py --turns 50`, bash then Rust back to back on the same machine
state (T1.4), compared with `bench/compare.py`. Rust is phase 1 (the agentd
binary of T1.3, built before the Codex core): Claude only, so its Codex rows only time the no-op
path, with sounds, notifications, background shells and the blink still run
by the bash helpers. Targets (design.md): p50 ≤ 5 ms, p99 ≤ 15 ms, one
resident process of ≤ 10 MB, blink ≤ 1% of a core.

## Quiet machine

- bash: Load average (1 min) 1.1 before, 1.3 after, 16 CPUs.
- rust: Load average (1 min) 1.3 before, 1.2 after, 16 CPUs.

Hook latency, ms (p50 / p99):

| Agent | Event | bash | rust |
|---|---|---|---|
| Claude | (calibration: /bin/true) | 0.5 / 11.4 | 0.5 / 0.9 |
| Claude | SessionStart | 21.9 / 21.9 | 6.6 / 6.6 |
| Claude | UserPromptSubmit | 23.5 / 31.9 | 7.4 / 8.6 |
| Claude | PreToolUse | 23.5 / 32.0 | 6.3 / 8.8 |
| Claude | PostToolUse | 20.4 / 28.3 | 6.0 / 16.6 |
| Claude | SubagentStart | 26.5 / 35.6 | 6.0 / 7.4 |
| Claude | SubagentStop | 21.4 / 27.0 | 6.0 / 7.8 |
| Claude | PermissionRequest | 30.7 / 36.4 | 6.1 / 9.0 |
| Claude | PostToolUse (answer) | 86.1 / 103.3 | 66.1 / 75.3 |
| Claude | Notification | 17.9 / 21.9 | 4.1 / 5.4 |
| Claude | PreCompact | 20.2 / 26.1 | 6.3 / 10.2 |
| Claude | PostCompact | 20.6 / 25.5 | 6.1 / 9.5 |
| Claude | Stop | 71.7 / 107.0 | 6.4 / 9.4 |
| Claude | SessionEnd | 76.5 / 76.5 | 61.8 / 61.8 |
| Codex | SessionStart | 134.4 / 134.4 | 4.6 / 4.6 |
| Codex | UserPromptSubmit | 134.7 / 215.9 | 5.0 / 6.3 |
| Codex | PreToolUse | 137.9 / 256.2 | 3.9 / 5.1 |
| Codex | PermissionRequest | 147.1 / 194.7 | 3.8 / 4.7 |
| Codex | PostToolUse | 196.8 / 249.0 | 3.7 / 13.4 |
| Codex | Stop | 172.9 / 221.2 | 3.8 / 5.8 |
| Codex | SessionEnd | 151.9 / 151.9 | 4.6 / 4.6 |

### bash

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 44.6 | 15.6 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 225.6 | 35.6 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 8.90 | 0.57 |
| nothing blinking | 0.00 | 0.00 |

### rust

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 4 | 20.0 | 5.5 | agentd ×1, bash ×2, sleep ×1 |
| after 20 waits on one pane | 1 | 4.2 | 1.9 | agentd ×1 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 9.03 | 0.53 |
| nothing blinking | 0.00 | 0.00 |

What it says: Claude events take 6-7 ms at p50 in Rust (bash 18-31), with
p99 under 10 ms but for one sample at 16.6. Leaving `needs` and SessionEnd
still cost ~60 ms: the synchronous `agent-notify --close` of the bash
helper, until phase 3. After 20 waits Rust keeps one process (4.2 MB RSS)
where bash keeps 40 (226 MB). The blink is the same bash animator in both.

## Loaded machine (load ~22)

Same comparison while the machine was busy with other work: absolute numbers
are inflated, the columns still compare.

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
