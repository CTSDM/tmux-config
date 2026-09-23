"""§7 Visibility (V1, V2), seen through its effects: H9 (done or idle), the
sounds that skip a visible pane (S1, S3) and notifications, which only go
out when the pane is away (N1)."""

import time
from collections.abc import Callable

import pytest

from harness.agent import FakeAgent
from harness.claude import state, working
from harness.marks import rule
from harness.tmux import TmuxServer
from harness.wait import eventually


def finish(server: TmuxServer, agent: FakeAgent) -> tuple[str, list[str], list[str]]:
    """Stop the turn; returns the state, the sounds and the notifications' urgencies."""
    working(agent)
    mark = server.sink.mark()
    agent.hook("Stop", last_assistant_message="finished")
    time.sleep(1.0)  # effects come after the hook returns
    lines = server.sink.since(mark)
    sounds = [line["name"] for line in lines if line["effect"] == "sound"]
    notes = [line["urgency"] for line in lines if line["effect"] == "notify" and line["pane"] == agent.pane]
    return state(agent), sounds, notes


@rule("V2", "H9", "S3", "N1")
def test_V2_visible_pane(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    assert finish(server, agent) == ("idle", [], [])


@rule("V2", "H9", "S3", "N1")
def test_V2_session_on_screen_other_window_in_front(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    agent = server.agent("claude")  # a background window of the client's session
    assert finish(server, agent) == ("done", ["enemy-down"], [])


@rule("V2", "H9", "S3", "N1")
def test_V2_session_on_screen_other_pane_in_front(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    front = server.tmux("display", "-p", "-t", "main", "#{pane_id}")
    agent = server.agent("claude", split=front)  # same window, not the active pane
    assert finish(server, agent) == ("done", ["enemy-down"], [])


@rule("V2", "H9", "S3", "N1")
def test_V2_client_on_another_session_is_away(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    assert server.client is not None
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    server.session("elsewhere")
    server.client.switch("elsewhere")
    assert finish(server, agent) == ("done", ["enemy-down"], ["normal"])


@rule("V2", "N1")
def test_V2_no_focused_client_is_away(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert finish(server, agent) == ("done", ["enemy-down"], ["normal"])


@rule("V1", "V2")
@pytest.mark.parametrize("focused", [True, False])
def test_V1_tmux_focus_flag_without_override(make_server: Callable[..., TmuxServer], focused: bool) -> None:
    server = make_server(focus="flag")
    client = server.client
    assert client is not None
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    if focused:
        client.focus_in()
    else:
        client.focus_out()
    eventually(
        lambda: "focused" in client.value("#{client_flags}").split(","),
        lambda flag: flag == focused,
        3,
        "client focus flag",
    )
    expected: tuple[str, list[str], list[str]] = ("idle", [], []) if focused else ("done", ["enemy-down"], ["normal"])
    assert finish(server, agent) == expected
