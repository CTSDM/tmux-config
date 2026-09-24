"""L1: the top row does not make the tmux server grow.

tmux 3.6 leaks a little memory on every #{S:}/#{W:}/#{P:} loop it expands
(tests/upstream/tmux-loop-leak.sh), and the bash-era top row expands many
per redraw: ~4 KB of server memory per redraw, for good. With agentd the
row reads plain values the daemon keeps instead (task L1)."""

import time
from collections.abc import Callable

import pytest

from harness.claude import working
from harness.impl import IMPL
from harness.tmux import Terminal, TmuxServer

GROWTH = 200  # bytes per redraw; tmux's own baseline is ~35, the looping row ~3700


@pytest.mark.skipif(IMPL.name != "rust", reason="the bash top row keeps its loops (L1 is agentd's)")
def test_L1_top_row_does_not_grow_the_server(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="terminal", theme=True)
    for name in ("alpha", "beta"):
        server.session(name)
    server.session("away", space="other")
    agents = [server.agent("claude", session) for session in ("main", "main", "alpha", "beta")]
    for agent in agents:
        working(agent)
    agents[0].hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})
    agents[1].hook("Stop")
    agents[2].hook("SubagentStart", agent_id="s", agent_type="Explore")
    server.run_tool([str(server.impl.bin / "agent-spaces"), "load"])
    time.sleep(2)
    assert isinstance(server.client, Terminal)
    assert "alpha" in server.client.screen()[0]  # the row is drawn
    growth = server.redraw_growth(server.client.name)
    assert growth < GROWTH, f"the tmux server grows {growth:.0f} bytes per redraw of the top row"
