"""Server memory growth per redraw of the bar, row by row (for L1).

    AGENT_IMPL=rust AGENTD=... uv run python bench/row_leak.py

The scene of contract/test_leak.py. Each row is measured alone, the other
one replaced by plain text of about the same length, then both together and
a plain bar (tmux's own baseline)."""

import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harness.agent import make_fakes  # noqa: E402
from harness.claude import working  # noqa: E402
from harness.impl import IMPL  # noqa: E402
from harness.tmux import Terminal, TmuxServer  # noqa: E402


def scene(fakes: dict[str, Path]) -> TmuxServer:
    root = Path(tempfile.mkdtemp(prefix="agt-rows-", dir="/tmp"))
    server = TmuxServer(root, fakes, focus="terminal", theme=True)
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
    return server


def main() -> None:
    fakes_dir = Path(tempfile.mkdtemp(prefix="agt-fakes-", dir="/tmp"))
    rows: list[tuple[str, float]] = []
    try:
        fakes = make_fakes(fakes_dir)
        for name, plain in (("both rows", ()), ("top row alone", (1,)), ("window row alone", (0,)),
                            ("plain bar", (0, 1))):
            server = scene(fakes)
            try:
                assert isinstance(server.client, Terminal)
                for row in plain:
                    width = len(server.client.screen()[row])
                    server.set_global(f"status-format[{row}]", "x" * max(width, 1))
                rows.append((name, server.redraw_growth(server.client.name)))
            finally:
                server.kill()
                shutil.rmtree(server.root, ignore_errors=True)
    finally:
        shutil.rmtree(fakes_dir, ignore_errors=True)
    print(f"Server RSS growth per redraw ({IMPL.name}):\n\n| Bar | Bytes per redraw |\n|---|---|")
    for name, growth in rows:
        print(f"| {name} | {growth:.0f} |")


if __name__ == "__main__":
    main()
