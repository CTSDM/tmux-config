"""§15 The bar (U1): clicks on a real client, rendered by an outer tmux.

theme.conf and agents.conf are loaded as tmux.conf does, with mouse on; the
top row shows the chips of the client's space, the second its windows."""

from collections.abc import Callable

from harness.marks import rule
from harness.tmux import Terminal, TmuxServer, column
from harness.wait import eventually


def bar(make_server: Callable[..., TmuxServer]) -> tuple[TmuxServer, Terminal]:
    """Sessions main, alpha and beta in the test space, zulu in another."""
    server = make_server(focus="terminal", theme=True)
    for name in ("alpha", "beta"):
        server.session(name)
    server.session("zulu", space="other")
    # What prefix+S does after a space changes: rebuild every session's rows.
    server.run_tool([str(server.impl.bin / "agent-spaces"), "load"])
    assert isinstance(server.client, Terminal)
    return server, server.client


def row(terminal: Terminal, index: int, *names: str) -> str:
    return eventually(lambda: terminal.screen()[index], lambda r: all(n in r for n in names), 5,
                      f"row {index + 1} shows {names}")


def session_of(terminal: Terminal) -> str:
    return terminal.value("#{client_session}")


@rule("U1", "C8")
def test_U1_click_a_session_chip(make_server: Callable[..., TmuxServer]) -> None:
    server, terminal = bar(make_server)
    top = row(terminal, 0, "alpha", "beta", "main")
    terminal.click(column(top, "alpha") + 1, 1)
    eventually(lambda: session_of(terminal), lambda s: s == "alpha", 3, "switched to alpha")
    # alpha's own row (same space): wait for the redraw, the current chip is wider.
    top = eventually(lambda: terminal.screen()[0], lambda r: r != top and "beta" in r, 5, "row redrawn")
    terminal.click(column(top, "beta") + 1, 1)
    eventually(lambda: session_of(terminal), lambda s: s == "beta", 3, "switched to beta")
    assert server.tmux("display", "-p", "-t", "beta", "#{session_attached}") == "1"


@rule("U1")
def test_U1_other_spaces_are_not_in_the_row(make_server: Callable[..., TmuxServer]) -> None:
    _, terminal = bar(make_server)
    top = row(terminal, 0, "alpha", "beta", "main")
    assert "zulu" not in top
    terminal.click(len(top) + 5, 1)  # past the last chip: nothing there
    assert session_of(terminal) == "main"


@rule("U1")
def test_U1_click_a_window_tab(make_server: Callable[..., TmuxServer]) -> None:
    server, terminal = bar(make_server)
    server.tmux("new-window", "-d", "-t", "main:", "-n", "tabtwo")
    tabs = row(terminal, 1, "tabtwo")
    terminal.click(column(tabs, "tabtwo") + 1, 2)
    eventually(lambda: terminal.value("#{window_name}"), lambda w: w == "tabtwo", 3, "window selected")
    assert session_of(terminal) == "main"
