# Benchmarks

```sh
cd tests
uv run python bench/run.py                    # bash, prints a Markdown report
uv run python bench/run.py --turns 100 --json bench/bash.json
AGENT_IMPL=rust AGENTD=../agentd/target/release/agentd uv run python bench/run.py
```

Same isolation as the contract suite (private `tmux -L` server, fake agents,
sink, tripwires). Run it on an otherwise quiet machine and not next to the
suite: the numbers are wall and CPU time.

What it measures (targets from [design.md](../../docs/daemon/design.md)):

- **Hook latency per event**: the time one hook call takes as its agent sees
  it, from process start to exit, measured inside the fake agent. Claude
  and Codex turns in a loop, alerts on (to the sink), every pane away, a
  0.3 s pause after each turn so detached helpers don't slow the next
  measure. Target: p50 ≤ 5 ms, p99 ≤ 15 ms.
- **Resident processes**: what the implementation leaves running, RSS and
  PSS (`smaps_rollup`), everything under the server's marker except tmux,
  the panes, the fake agents and what they started. Two scenes: four agents
  (waiting, done with a background shell, done, Codex working), and one pane
  after 20 waits (bash keeps a reminder per wait, CHANGE C1). Target: one
  process, RSS ≤ 10 MB.
- **CPU**: % of one core of the implementation's processes (their reaped
  children included) and of the tmux server, while a pane blinks as `needs`
  and while nothing blinks. No client is attached, so tmux does not redraw.
  Target: ≤ 1% blinking, 0 idle.

[baseline-bash.md](baseline-bash.md) has the reference numbers.
