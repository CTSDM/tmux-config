"""§1 Inputs: invocation (I1), robustness (I2), fields (I3), ownership (I4)."""

import json
from pathlib import Path
from typing import Any

import pytest

from harness.agent import FakeAgent
from harness.claude import shown, state, working
from harness.impl import IMPL
from harness.marks import change, rule
from harness.tmux import TmuxServer

# An event that, if handled, writes options and alerts (sound, notification).
ALERTING = {"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_use_id": "c1",
            "tool_input": {"command": "ls"}}


def assert_ignored(server: TmuxServer, agent: FakeAgent, run: Any) -> None:
    """`run()` must change nothing: no option of any pane, nothing in the sink."""
    panes = server.tmux("list-panes", "-a", "-F", "#{pane_id}").splitlines()
    before = {p: server.pane_options(p) for p in panes}
    mark = server.sink.mark()
    run()
    assert {p: server.pane_options(p) for p in panes} == before
    server.sink.quiet(1.0, after=mark)


# --- I1 ---------------------------------------------------------------------


@rule("I1")
@pytest.mark.parametrize("kind", [None, "", "Claude", "claude-code", "gemini"])
def test_I1_kind_must_be_claude_or_codex(server: TmuxServer, kind: str | None) -> None:
    agent = server.agent("claude")
    argv = IMPL.hook_argv("claude")[:-1] + ([] if kind is None else [kind])
    assert_ignored(server, agent, lambda: agent.run_hook(json.dumps(ALERTING), argv=argv))
    assert agent.options() == {}


@rule("I1")
@pytest.mark.parametrize("var", ["TMUX", "TMUX_PANE"])
def test_I1_needs_TMUX_and_TMUX_PANE(server: TmuxServer, var: str) -> None:
    agent = server.agent("claude")
    assert_ignored(server, agent, lambda: agent.run_hook(json.dumps(ALERTING), env={var: None}))
    assert agent.options() == {}


# --- I2 ---------------------------------------------------------------------


@rule("I2")
@pytest.mark.parametrize(
    "payload", ["", "not json", "{", '{"hook_event_name":', "null", "[1, 2]", '"Stop"', "42"]
)
def test_I2_bad_payload_does_nothing(server: TmuxServer, payload: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert_ignored(server, agent, lambda: agent.run_hook(payload))


@rule("I2")
def test_I2_tmux_gone(server: TmuxServer, tmp_path: Path) -> None:
    agent = server.agent("claude")
    working(agent)
    gone = {"TMUX": f"{tmp_path}/no-such-server,1,0"}
    result = agent.run_hook(json.dumps(ALERTING), env=gone)
    assert result.ms < 1000, f"took {result.ms:.0f} ms"
    assert state(agent) == "working"


@rule("I2")
def test_I2_pane_gone(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert_ignored(
        server, agent, lambda: agent.run_hook(json.dumps(ALERTING), env={"TMUX_PANE": "%999"})
    )


@rule("I2")
def test_I2_returns_well_under_a_second(server: TmuxServer) -> None:
    agent = server.agent("claude")
    events: list[dict[str, Any]] = [
        {"hook_event_name": "SessionStart", "source": "startup"},
        {"hook_event_name": "UserPromptSubmit"},
        {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "a",
         "tool_input": {"command": "ls"}},
        ALERTING,
        {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "c1"},
        {"hook_event_name": "Stop", "last_assistant_message": "done"},
        {"hook_event_name": "SessionEnd"},
    ]
    for event in events:
        result = agent.run_hook(json.dumps({"session_id": agent.session_id, **event}))
        assert result.ms < 1000, f"{event['hook_event_name']} took {result.ms:.0f} ms"


# --- I3 ---------------------------------------------------------------------


def stop_message(agent: FakeAgent, last: Any) -> str:
    working(agent)
    agent.hook("Stop", last_assistant_message=last)
    return agent.option("@agent_msg")


def tool_label(agent: FakeAgent, tool: Any, tool_input: Any) -> str:
    agent.hook("PreToolUse", tool_name=tool, tool_use_id="t1", tool_input=tool_input)
    return agent.option("@agent_tool")


@rule("I3")
def test_I3_whitespace_runs_collapse(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert stop_message(agent, "  one\n\n\ttwo   three \r\n") == " one two three "


@rule("I3")
def test_I3_cut_to_300_characters_not_bytes(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert stop_message(agent, "é" * 400) == "é" * 300


@rule("I3")
def test_I3_cut_after_collapsing(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert stop_message(agent, "x\n\n\n\n" * 100) == "x " * 100


@rule("I3")
def test_I3_values_become_strings(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert stop_message(agent, 42) == "42"
    agent.hook("UserPromptSubmit", session_id=7)
    assert agent.option("@agent_session") == "7"


@rule("I3", "H1c")
def test_I3_null_is_empty(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup", model=None)
    assert "@agent_model" not in shown(agent)
    assert tool_label(agent, "Read", None) == "Read"
    assert stop_message(agent, None) == ""


@rule("I3")
@pytest.mark.parametrize(
    ("tool_input", "label"),
    [
        ({"command": "c", "file_path": "f", "description": "d"}, "T: c"),
        ({"file_path": "f", "path": "p"}, "T: f"),
        ({"path": "p", "pattern": "x"}, "T: p"),
        ({"pattern": "x", "url": "u"}, "T: x"),
        ({"url": "u", "query": "q"}, "T: u"),
        ({"query": "q", "description": "d"}, "T: q"),
        ({"description": "d", "other": "o"}, "T: d"),
        ({"other": "o"}, "T"),
        ({}, "T"),
        ("just text", "T: just text"),
        ([1, 2], "T: [1,2]"),
    ],
)
def test_I3_detail_and_label(server: TmuxServer, tool_input: Any, label: str) -> None:
    agent = server.agent("claude")
    assert tool_label(agent, "T", tool_input) == label


@rule("I3")
@pytest.mark.parametrize(
    ("tool_input", "label"),
    [
        ({"command": None, "file_path": "f"}, "T: f"),
        ({"command": False, "file_path": "f"}, "T: f"),
        ({"command": "", "file_path": "f"}, "T"),  # present and not null: it wins
        ({"command": 0, "file_path": "f"}, "T: 0"),
        ({"command": None}, "T"),
        (None, "T"),
        (False, "T"),
        (7, "T: 7"),
        (True, "T: true"),
    ],
)
def test_I3_detail_takes_the_first_key_neither_null_nor_false(
    server: TmuxServer, tool_input: Any, label: str
) -> None:
    agent = server.agent("claude")
    assert tool_label(agent, "T", tool_input) == label


@rule("I3")
def test_I3_detail_is_collapsed_and_cut_on_its_own(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert tool_label(agent, "Bash", {"command": "a\n   b"}) == "Bash: a b"
    assert tool_label(agent, "Bash", {"command": "y" * 400}) == "Bash: " + "y" * 300


@rule("I3")
def test_I3_hash_is_written_doubled_after_the_cut(server: TmuxServer) -> None:
    agent = server.agent("claude")
    assert tool_label(agent, "Bash", {"command": "echo #1"}) == "Bash: echo ##1"
    assert stop_message(agent, "#" * 400) == "#" * 600


# --- I4 ---------------------------------------------------------------------


@rule("I4")
def test_I4_agent_is_the_pane_process(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup")
    assert state(agent) == "ready"


@rule("I4")
def test_I4_agent_under_the_pane_shell(server: TmuxServer) -> None:
    agent = server.agent("claude", shell=True)
    agent.hook("SessionStart", source="startup")
    assert state(agent) == "ready"


# The hook's parent is the 1st process of the walk; `_wrap=N` puts N shells
# first, so an agent that is the pane's process comes (N+1)th.


@rule("I4")
@pytest.mark.parametrize("depth", [1, 5, 11])
def test_I4_hook_through_intermediate_processes(server: TmuxServer, depth: int) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup", _wrap=depth)
    assert state(agent) == "ready"


@rule("I4")
def test_I4_pane_shell_13th(server: TmuxServer) -> None:
    agent = server.agent("claude", shell=True)  # agent 12th, pane shell 13th
    agent.hook("SessionStart", source="startup", _wrap=11)
    assert state(agent) == "ready"


@rule("I4", "C4")
@change("C4")
def test_I4_pane_agent_13th_is_counted(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup", _wrap=12)
    assert state(agent) == "ready"


@rule("I4", "C4")
@change("C4")
def test_I4_two_agents_with_the_pane_13th(server: TmuxServer) -> None:
    agent = server.agent("claude")  # 13th
    agent.hook("SessionStart", source="startup")
    agent.hook("UserPromptSubmit")
    nested = agent.child_agent("claude")  # 12th
    assert_ignored(server, agent, lambda: nested.run_hook(json.dumps(ALERTING), wrap=11))


@rule("I4")
@pytest.mark.parametrize("depth", [13, 20])
def test_I4_pane_beyond_13_processes_is_ignored(server: TmuxServer, depth: int) -> None:
    agent = server.agent("claude")
    assert_ignored(server, agent, lambda: agent.hook("SessionStart", source="startup", _wrap=depth))


@rule("I4")
@pytest.mark.parametrize(("outer", "inner"), [("claude", "claude"), ("claude", "codex"),
                                              ("codex", "claude")])
def test_I4_agent_started_by_the_agent_is_ignored(server: TmuxServer, outer: str, inner: str) -> None:
    agent = server.agent(outer)
    agent.hook("SessionStart", source="startup")
    agent.hook("UserPromptSubmit")
    nested = agent.child_agent(inner)
    assert_ignored(server, agent, lambda: nested.run_hook(json.dumps(ALERTING)))
    assert_ignored(server, agent, lambda: nested.hook("Stop", last_assistant_message="x"))


@rule("I4")
def test_I4_no_agent_in_the_chain_is_ignored(server: TmuxServer) -> None:
    tool = server.agent("node")
    assert_ignored(server, tool, lambda: tool.run_hook(json.dumps(ALERTING), kind="claude"))


@rule("I4")
def test_I4_agent_of_another_pane_is_ignored(server: TmuxServer) -> None:
    mine = server.agent("claude")
    working(mine)
    other = server.agent("claude")
    assert_ignored(
        server, mine, lambda: other.run_hook(json.dumps(ALERTING), env={"TMUX_PANE": mine.pane})
    )


@rule("I3", "C9")
@change("C9")
@pytest.mark.parametrize("value", ["echo hi;", "a ; b ;", r"find . -exec rm {} \;"])
def test_C9_values_are_stored_whole(server: TmuxServer, value: str) -> None:
    """A trailing `;` is part of the value, not a tmux command separator."""
    agent = server.agent("claude")
    assert tool_label(agent, "Bash", {"command": value}) == f"Bash: {value}"
    assert stop_message(agent, value) == value
