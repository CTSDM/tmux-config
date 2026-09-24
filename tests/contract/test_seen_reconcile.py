"""§12 Seen (E1) and reconciliation (E2) for Claude panes; Codex is in §6."""

import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from harness.agent import FakeAgent
from harness.claude import ask, near_now, next_second, shown, state, working
from harness.marks import change, rule
from harness.tmux import TmuxServer
from harness.wait import eventually

# Synthetic transcript entries (only the fields E2 looks at, plus some noise).
TURN_END: dict[str, Any] = {"type": "system", "subtype": "turn_duration", "durationMs": 1234}
OTHER_SYSTEM: dict[str, Any] = {"type": "system", "subtype": "compact_boundary"}
NOISE: dict[str, Any] = {"type": "file-history-snapshot", "snapshot": {}}


def user(text: str, blocks: bool = False) -> dict[str, Any]:
    content: Any = [{"type": "text", "text": text}] if blocks else text
    return {"type": "user", "message": {"role": "user", "content": content}}


def assistant(text: str) -> dict[str, Any]:
    return {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}


def transcript(path: Path, *entries: dict[str, Any] | str) -> str:
    path.write_text("".join((e if isinstance(e, str) else json.dumps(e)) + "\n" for e in entries))
    return str(path)


def in_front(server: TmuxServer, agent: FakeAgent) -> None:
    server.tmux("select-window", "-t", agent.pane)


def focused_server(make_server: Callable[..., TmuxServer]) -> TmuxServer:
    server = make_server(focus="client")
    assert server.client is not None
    server.client.focus_in()
    return server


# --- E1 seen ---------------------------------------------------------------------


@rule("E1")
def test_E1_looking_at_done_work_makes_it_idle(make_server: Callable[..., TmuxServer]) -> None:
    server = focused_server(make_server)
    agent = server.agent("claude")  # behind another window of the client's session
    working(agent)
    agent.hook("Stop")
    since = shown(agent)["@agent_since"]
    assert state(agent) == "done"
    next_second()
    in_front(server, agent)
    eventually(lambda: state(agent), lambda s: s == "idle", 3, "seen")
    assert shown(agent)["@agent_since"] == since


@rule("E1", "N3")
@pytest.mark.parametrize("wait", [False, True])
def test_E1_looking_closes_the_notification(make_server: Callable[..., TmuxServer], wait: bool) -> None:
    server = focused_server(make_server)
    client = server.client
    assert client is not None
    agent = server.agent("claude")
    in_front(server, agent)
    server.session("elsewhere")
    client.switch("elsewhere")  # away
    working(agent)
    if wait:
        ask(agent, "Bash", "a")
    else:
        agent.hook("Stop")
    server.sink.wait_for(lambda line: line["effect"] == "notify" and line["pane"] == agent.pane)
    mark = server.sink.mark()
    client.switch("main")
    server.sink.wait_for(
        lambda line: line["effect"] == "notify-close" and line["pane"] == agent.pane, after=mark
    )
    assert state(agent) == ("needs" if wait else "idle")


@rule("E1")
@pytest.mark.parametrize("current", ["working", "error"])
def test_E1_other_states_stay(make_server: Callable[..., TmuxServer], current: str) -> None:
    server = focused_server(make_server)
    agent = server.agent("claude")
    working(agent)
    if current == "error":
        agent.hook("StopFailure", error="x")
    in_front(server, agent)
    time.sleep(1)
    assert state(agent) == current


# --- E2 agent gone ------------------------------------------------------------------


@rule("E2", "P1")
@pytest.mark.parametrize("shell", [False, True])
def test_E2_agent_gone_clears_the_pane(server: TmuxServer, shell: bool) -> None:
    agent = server.agent("claude", shell=shell)
    working(agent)
    agent.hook("Stop", last_assistant_message="bye")
    agent.exit()
    server.reconcile(agent.pane)
    assert shown(agent) == {}


@rule("E2", "P1", "N3", "C6")
@change("C6")
def test_E2_agent_gone_closes_its_notification(server: TmuxServer) -> None:
    """Like H11: the notification closes, and no option is left, internal
    ones included."""
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop", last_assistant_message="bye")
    server.sink.wait_for(lambda line: line["effect"] == "notify")
    time.sleep(0.5)
    agent.exit()
    mark = server.sink.mark()
    server.reconcile(agent.pane)
    server.sink.wait_for(
        lambda line: line["effect"] == "notify-close" and line["pane"] == agent.pane, 3, after=mark
    )
    assert agent.options() == {}


@rule("E2")
def test_E2_live_agent_under_a_shell_stays(server: TmuxServer) -> None:
    agent = server.agent("claude", shell=True)
    working(agent)
    agent.hook("Stop")
    before = shown(agent)
    server.reconcile(agent.pane)
    assert shown(agent) == before


@rule("E2")
def test_E2_every_agent_pane_by_default(server: TmuxServer) -> None:
    gone, alive = server.agent("claude"), server.agent("claude")
    for agent in (gone, alive):
        working(agent)
    gone.exit()
    server.reconcile()
    assert gone.options() == {}
    assert state(alive) == "working"


# --- E2 transcript says the turn is over -----------------------------------------------


OVER: list[tuple[str, list[dict[str, Any] | str]]] = [
    ("turn duration", [user("hi"), assistant("hello"), TURN_END]),
    ("interrupted", [user("hi"), assistant("hel"), user("[Request interrupted by user]")]),
    ("interrupted, blocks", [user("hi"), user("[Request interrupted by user for tool use]", blocks=True)]),
    ("other system entries skipped", [user("hi"), TURN_END, OTHER_SYSTEM, NOISE]),
    ("bad lines skipped", [user("hi"), TURN_END, "{not json", NOISE]),
    ("within the last 80 lines", [user("hi"), TURN_END, *[NOISE] * 78]),
]
BUSY: list[tuple[str, list[dict[str, Any] | str]]] = [
    ("assistant last", [user("hi"), assistant("working on it")]),
    ("new turn after the end", [user("hi"), TURN_END, user("more")]),
    ("user text mentioning an interruption", [user("hi"), user("Why [Request interrupted by user]?")]),
    ("end beyond the last 80 lines", [user("hi"), TURN_END, *[NOISE] * 80]),
    ("empty", []),
]


@rule("E2")
@pytest.mark.parametrize(("case", "entries"), OVER, ids=[c for c, _ in OVER])
def test_E2_turn_over_is_idle(server: TmuxServer, tmp_path: Path, case: str,
                              entries: list[dict[str, Any] | str]) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path=transcript(tmp_path / "t.jsonl", *entries))
    agent.hook("PreToolUse", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})
    next_second()
    mark = server.sink.mark()
    server.reconcile(agent.pane)
    options = shown(agent)
    assert options["@agent_state"] == "idle"
    assert near_now(options["@agent_since"]) and "@agent_tool" not in options
    server.sink.quiet(1.0, after=mark)


@rule("E2")
@pytest.mark.parametrize(("case", "entries"), BUSY, ids=[c for c, _ in BUSY])
def test_E2_turn_not_over_stays(server: TmuxServer, tmp_path: Path, case: str,
                                entries: list[dict[str, Any] | str]) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path=transcript(tmp_path / "t.jsonl", *entries))
    before = shown(agent)
    server.reconcile(agent.pane)
    assert shown(agent) == before


@rule("E2")
def test_E2_unreadable_transcript_is_not_over(server: TmuxServer, tmp_path: Path) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path=str(tmp_path / "missing.jsonl"))
    server.reconcile(agent.pane)
    assert state(agent) == "working"


@rule("E2")
@pytest.mark.parametrize("current", ["needs", "compacting"])
def test_E2_other_busy_states(server: TmuxServer, tmp_path: Path, current: str) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path=transcript(tmp_path / "t.jsonl", user("hi"), TURN_END))
    if current == "needs":
        ask(agent, "Bash", "a")
    else:
        agent.hook("PreCompact")
    server.reconcile(agent.pane)
    options = shown(agent)
    assert options["@agent_state"] == "idle"
    assert "@agent_needs" not in options and "@agent_needs_id" not in options


@rule("E2")
@pytest.mark.parametrize("current", ["done", "ready"])
def test_E2_calm_states_stay(server: TmuxServer, tmp_path: Path, current: str) -> None:
    agent = server.agent("claude")
    path = transcript(tmp_path / "t.jsonl", user("hi"), TURN_END)
    agent.hook("SessionStart", source="startup", transcript_path=path)
    if current == "done":
        agent.hook("UserPromptSubmit")
        agent.hook("Stop")
    server.reconcile(agent.pane)
    assert state(agent) == current


# --- E2 triggers -----------------------------------------------------------------------


@rule("E2")
def test_E2_leaving_a_busy_pane_reconciles_it(make_server: Callable[..., TmuxServer], tmp_path: Path) -> None:
    server = focused_server(make_server)
    agent = server.agent("claude")
    in_front(server, agent)
    working(agent, transcript_path=transcript(tmp_path / "t.jsonl", user("hi"), TURN_END))
    server.tmux("select-window", "-t", "main:^")  # focus leaves the pane
    eventually(lambda: state(agent), lambda s: s == "idle", 5, "reconciled on focus out")


@rule("E2")
def test_E2_loading_the_configuration_reconciles(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.exit()
    server.tmux("source-file", str(server.impl.conf))
    eventually(agent.options, lambda o: o == {}, 5, "cleared on configuration load")
