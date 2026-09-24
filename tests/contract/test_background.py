"""§9 Background shells of Claude (B1, B2), and how they show (N2, A4)."""

import time

from harness.agent import FakeAgent
from harness.claude import shown, working
from harness.marks import rule
from harness.tmux import TmuxServer

WATCH = 5.0  # "checked about every 2 s", with margin

# What Claude's Bash tool runs for run_in_background: a shell that sources
# the session's shell snapshot first.
SHELL = "source /nonexistent/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 300; exit 0"


def background_shell(agent: FakeAgent) -> int:
    return agent.spawn(["/bin/bash", "-c", SHELL])


def bg(agent: FakeAgent) -> str:
    return shown(agent).get("@agent_bg", "")


def wait_bg(agent: FakeAgent, value: str, timeout: float = WATCH) -> None:
    end = time.monotonic() + timeout
    while bg(agent) != value:
        assert time.monotonic() < end, f"@agent_bg is {bg(agent)!r}, expected {value!r}"
        time.sleep(0.1)


@rule("B1")
def test_B1_shells_left_running_at_stop(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    shells = [background_shell(agent), background_shell(agent)]
    agent.hook("Stop")
    wait_bg(agent, "2")
    agent.signal(shells[0])
    wait_bg(agent, "1")
    agent.signal(shells[1])
    wait_bg(agent, "")
    assert "@agent_bg" not in agent.options()  # unset, not 0


@rule("B1")
def test_B1_no_shells_no_count(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop")
    time.sleep(2.5)
    assert bg(agent) == ""


@rule("B1")
def test_B1_only_snapshot_shells_count(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.spawn(["/bin/sh", "-c", "sleep 300; exit 0", "mcp-server"])  # an MCP server
    # A snapshot shell that is a grandchild, not a child, of the agent.
    agent.spawn(["/bin/sh", "-c", 'bash -c "$CMD"; exit 0'], env={"CMD": SHELL})
    agent.hook("Stop")
    time.sleep(2.5)
    assert bg(agent) == ""


@rule("B1")
def test_B1_counted_at_stop_only(server: TmuxServer) -> None:
    """Shells that start after the turn ended are not looked for (B2 does)."""
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop")
    background_shell(agent)
    time.sleep(3)
    assert bg(agent) == ""


@rule("B2", "E2")
def test_B2_reconcile_starts_counting(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop")
    shell = background_shell(agent)
    server.reconcile(agent.pane)
    wait_bg(agent, "1")
    agent.signal(shell)
    wait_bg(agent, "")


@rule("B2", "E2")
def test_B2_reconcile_of_every_pane(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop")
    background_shell(agent)
    server.reconcile()
    wait_bg(agent, "1")


@rule("N2", "B1")
def test_N2_done_mentions_the_shells(server: TmuxServer) -> None:
    agent = server.agent("claude")
    for count, suffix in ((1, " · 1 shell still running"), (2, " · 2 shells still running")):
        working(agent)
        shells = [background_shell(agent) for _ in range(count)]
        mark = server.sink.mark()
        agent.hook("Stop", last_assistant_message="Started the server.")
        note = server.sink.wait_for(lambda line: line["effect"] == "notify", after=mark)
        assert note["body"] == "Started the server." + suffix
        for shell in shells:
            agent.signal(shell)
        wait_bg(agent, "")


@rule("A4", "B1")
def test_A4_background_glyph_while_shells_run(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    shell = background_shell(agent)
    agent.hook("Stop")
    wait_bg(agent, "1")
    glyph = lambda: server.tmux("display", "-p", "-t", agent.pane, "#{E:@agent-glyph}")  # noqa: E731
    assert "◐" in glyph()
    agent.signal(shell)
    wait_bg(agent, "")
    assert "✓" in glyph()
