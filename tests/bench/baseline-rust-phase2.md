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
