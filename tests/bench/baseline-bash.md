# Baseline: bash

Reference numbers for the bash implementation (agents/bin of `daemon` with
the T0.2 seams), from `uv run python bench/run.py --turns 50` on a quiet
machine (load average ~2 on 16 CPUs). The calibration row (starting
`/bin/true` the same way a hook is started) is there to compare runs: under
heavy load (~20) the same run gave about three times these latencies, so
compare implementations back to back, on the same machine state.

What the numbers show:

- Every Claude event pays for a bash script with `jq` and 2-3 `tmux` calls:
  ~20-30 ms.
- Leaving `needs` (the PostToolUse that answers a PermissionRequest) and
  Stop cost ~70-100 ms: the hook runs `agent-notify --close`, a uv-started
  Python, synchronously.
- Codex events add `agent-codex prepare` (Python) to every call: ~140-210 ms.
- Bash keeps one sleeping `agent-remind` (bash + `sleep`) per wait, never
  cancelled (CHANGE C1): 20 waits leave 40 processes, ~225 MB RSS.
- The blink animator starts a `tmux` process per frame: ~9% of a core.

Load average (1 min) 1.5 before, 2.5 after, 16 CPUs.

Hook latency, Claude (ms, 50 turns):

| Event | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| (calibration: /bin/true) | 50 | 0.5 | 0.6 | 0.9 | 0.9 |
| SessionStart | 1 | 21.6 | 21.6 | 21.6 | 21.6 |
| UserPromptSubmit | 50 | 24.0 | 28.3 | 34.8 | 34.8 |
| PreToolUse | 50 | 23.6 | 28.5 | 32.2 | 32.2 |
| PostToolUse | 50 | 20.7 | 24.8 | 28.2 | 28.2 |
| SubagentStart | 50 | 26.7 | 32.7 | 39.7 | 39.7 |
| SubagentStop | 50 | 21.2 | 25.2 | 34.6 | 34.6 |
| PermissionRequest | 50 | 30.9 | 35.9 | 49.0 | 49.0 |
| PostToolUse (answer) | 50 | 95.2 | 107.2 | 153.3 | 153.3 |
| Notification | 50 | 18.0 | 21.6 | 25.4 | 25.4 |
| PreCompact | 50 | 20.5 | 25.7 | 29.6 | 29.6 |
| PostCompact | 50 | 20.6 | 24.8 | 32.1 | 32.1 |
| Stop | 50 | 71.3 | 84.0 | 102.6 | 102.6 |
| SessionEnd | 1 | 96.3 | 96.3 | 96.3 | 96.3 |

Hook latency, Codex (ms, 50 turns):

| Event | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| SessionStart | 1 | 142.4 | 142.4 | 142.4 | 142.4 |
| UserPromptSubmit | 50 | 137.7 | 150.0 | 201.3 | 201.3 |
| PreToolUse | 50 | 141.5 | 216.3 | 234.6 | 234.6 |
| PermissionRequest | 50 | 152.6 | 175.5 | 193.1 | 193.1 |
| PostToolUse | 50 | 211.3 | 226.4 | 273.2 | 273.2 |
| Stop | 50 | 180.3 | 189.5 | 219.7 | 219.7 |
| SessionEnd | 1 | 177.5 | 177.5 | 177.5 | 177.5 |

Resident processes of the implementation:

| Scene | Processes | RSS MB | PSS MB | By name |
|---|---|---|---|---|
| steady (4 agents) | 6 | 44.7 | 14.4 | bash ×3, python3 ×1, sleep ×2 |
| after 20 waits on one pane | 40 | 225.8 | 35.2 | bash ×20, sleep ×20 |

CPU, % of one core (30 s):

| Scene | Implementation | tmux server |
|---|---|---|
| needs blinking | 9.10 | 0.53 |
| nothing blinking | 0.00 | 0.00 |
