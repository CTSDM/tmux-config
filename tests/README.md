# Contract suite

Black-box tests of the agent layer against
[docs/daemon/contract.md](../docs/daemon/contract.md). The same tests run
against the bash implementation (the reference) and against `agentd`.

```sh
cd tests
uv run pytest                          # bash: ../agents/bin/agent-hook
uv run pytest -n 4 --dist loadgroup    # in parallel (pytest-xdist): ~3 min instead of ~11
AGENT_IMPL=rust AGENTD=../agentd/target/release/agentd uv run pytest
uv run pyright                         # strict
```

| Variable | Meaning |
|---|---|
| `AGENT_IMPL` | `bash` (default) or `rust` |
| `AGENT_BASH_BIN` | bash helpers to test (default `../agents/bin`) |
| `AGENTD` | the `agentd` binary, for `rust` |
| `AGENT_CONF` | tmux configuration sourced into each test server (default `agents.conf` next to the bash bin directory) |
| `AG_TEST_TMPDIR` | where temp dirs go (default `/tmp`; Unix socket paths must stay short) |
| `AG_TEST_KEEP=1` | keep each test's temp dir (sink, logs, `state/tmux-agents/`) |

Every test has its own server, runtime dir and sound debounce, so tests
are independent and the suite must pass the same in series and with `-n`.
Timing-sensitive tests (debounce, blink frames, reminders, Codex ticks)
keep margins for that; none is retried.

Layout: `harness/` (fixtures' machinery), `selftest/` (the harness itself),
`contract/` (one module per contract section, tests named and marked with the
rule ids they cover), `bench/` (latency, memory, CPU).

## What a test gets

- `server` / `make_server(focus=..., conf=...)`: a `TmuxServer`, a private
  `tmux -L agtest-<pid>-<n>` with a session `main`, killed after the test with
  every process it started. `focus="none"` (default) makes every pane `away`;
  `"client"` attaches a real client to `main` on a pty and names it in
  `AG_FOCUS_CLIENT`; `"flag"` attaches it but leaves `AG_FOCUS_CLIENT` unset,
  so tmux's `focused` flag decides (`client.focus_in()` / `focus_out()` send
  the terminal's focus reports). `"terminal"` attaches it from a pane of a
  second tmux server, which renders it: `client.screen()` reads it and
  `client.click(x, y)` sends an SGR mouse click (the bar tests, with
  `theme=True`, which sources theme.conf and turns the mouse on).
- `server.agent("claude" | "codex", session, shell=..., split=...)`: a
  `FakeAgent` in a new pane. It is a copy of the Python interpreter named
  `claude` or `codex` (so `/proc/<pid>/comm` matches) running
  `harness/fake_agent.py`, which the test drives over a Unix socket.
  `agent.hook("Stop", last_assistant_message="x")` runs the implementation's
  hook as a child of that process with the payload on stdin, and checks
  contract I2 (exit 0, no stdout, no timeout). `agent.spawn(argv, env=...,
  new_session=...)` starts other children (background shells, MCP-like
  processes); `agent.child_agent()` a nested agent; `agent.exit()` ends it.
- `agent.options()` / `server.pane_options(pane)`: the pane's user options,
  raw (`##` as stored). Also `window_options`, `session_options`,
  `global_option`, `set_global`, `wait_option`.
- `server.sink`: the `AG_SINK` lines (`sounds()`, `notifications(pane)`,
  `wait_for(...)`, `mark()` / `since(mark)`, `quiet(seconds)`).
- `server.reconcile(*panes)`, `server.ensure()`: implementation commands run
  from the test process against the test server.

## Safety

Each server runs from a clean environment: no `TMUX`/`TMUX_PANE`, temporary
`HOME`, `XDG_*` dirs and sounds dir, and the seams `AG_SINK` and
`AG_FOCUS_CLIENT` in the server's global environment. Second line of
defense: `AG_SOUND_PLAYER` is a script that only records its call, and
`DBUS_SESSION_BUS_ADDRESS` is unset with a recording socket at
`$XDG_RUNTIME_DIR/bus`, where D-Bus clients then look. A test that trips
either fails. Every process started under the server carries
`AG_TEST_RUN=<marker>`; teardown kills the server by `-L` name and then
exactly the processes with that marker, by pid.
