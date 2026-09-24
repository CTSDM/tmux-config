"""§5 Subagents (A1-A4)."""

import subprocess
from typing import Any

import pytest

from harness.agent import FakeAgent
from harness.claude import shown, state, working
from harness.marks import rule
from harness.tmux import TmuxServer


def start(agent: FakeAgent, agent_id: str, agent_type: Any = "Explore") -> None:
    agent.hook("SubagentStart", agent_id=agent_id, agent_type=agent_type)


def stop(agent: FakeAgent, agent_id: str, agent_type: Any = "Explore") -> None:
    agent.hook("SubagentStop", agent_id=agent_id, agent_type=agent_type)


def counts(agent: FakeAgent) -> tuple[str, str]:
    options = shown(agent)
    return options.get("@agent_subs", "0"), options.get("@agent_subtypes", "")


@rule("A1", "A2")
def test_A1_start_and_stop(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x")
    assert counts(agent) == ("1", "1 Explore")
    start(agent, "y", "Plan")
    assert counts(agent) == ("2", "1 Explore, 1 Plan")
    stop(agent, "x")
    assert counts(agent) == ("1", "1 Plan")
    stop(agent, "y", "Plan")
    assert counts(agent) == ("0", "")
    assert state(agent) == "working"


@rule("A1")
def test_A1_the_same_subagent_counts_once(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x")
    start(agent, "x")
    assert counts(agent) == ("1", "1 Explore")


@rule("A1")
def test_A1_stopping_an_unknown_subagent_changes_no_count(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x")
    stop(agent, "never-started")
    assert counts(agent) == ("1", "1 Explore")


@rule("A1", "A2")
@pytest.mark.parametrize("agent_type", ["", None, "default"])
def test_A1_untyped_subagents_are_agent(server: TmuxServer, agent_type: Any) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x", agent_type)
    assert counts(agent) == ("1", "1 agent")


@rule("A2")
def test_A2_types_counted_and_sorted_by_name(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    for agent_id, agent_type in [("1", "Review"), ("2", "Explore"), ("3", "Plan"), ("4", "Explore")]:
        start(agent, agent_id, agent_type)
    assert counts(agent) == ("4", "2 Explore, 1 Plan, 1 Review")


@rule("A2")
def test_A2_sorted_ignoring_case(server: TmuxServer) -> None:
    """Bash sorts under the user's locale, so this test's hooks run with
    LANG=en_US.UTF-8, like the live system."""
    locales = subprocess.run(["locale", "-a"], capture_output=True, text=True).stdout.split()
    if "en_US.utf8" not in locales:
        pytest.skip("locale en_US.UTF-8 is not installed")
    agent = server.agent("claude")
    working(agent)
    lang = {"LANG": "en_US.UTF-8"}
    for agent_id, agent_type in [("1", "Plan"), ("2", "Explore"), ("3", ""), ("4", "zeta")]:
        agent.hook("SubagentStart", agent_id=agent_id, agent_type=agent_type, _env=lang)
    assert counts(agent) == ("4", "1 agent, 1 Explore, 1 Plan, 1 zeta")


@rule("A1", "H14")
@pytest.mark.parametrize(
    "event",
    ["UserPromptSubmit", "PreToolUse", "PermissionRequest", "PostToolUse", "Stop", "StopFailure",
     "Notification", "PreCompact", "SessionStart", "SessionEnd"],
)
def test_A1_other_subagent_events_are_ignored(server: TmuxServer, event: str) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x")
    before = agent.options()
    agent.hook(
        event,
        agent_id="x",
        agent_type="Explore",
        tool_name="AskUserQuestion",
        tool_use_id="q",
        notification_type="permission_prompt",
        last_assistant_message="sub done",
        error="sub failed",
        source="startup",
        permission_mode="plan",
        transcript_path="/tmp/sub.jsonl",
        model="sub-model",
    )
    assert agent.options() == before


@rule("A1", "H14")
def test_A1_subagent_start_and_stop_keep_the_main_identity(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path="/tmp/main.jsonl", permission_mode="default")
    before = {k: v for k, v in shown(agent).items() if k not in ("@agent_subs", "@agent_subtypes")}
    for event in ("SubagentStart", "SubagentStop"):
        agent.hook(event, agent_id="x", agent_type="Explore",
                   transcript_path="/tmp/sub.jsonl", permission_mode="plan", model="sub-model")
        after = {k: v for k, v in shown(agent).items() if k not in ("@agent_subs", "@agent_subtypes")}
        assert after == before


@rule("A1")
def test_A1_subagents_outlive_the_main_turn(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    start(agent, "x")
    agent.hook("Stop", last_assistant_message="waiting on a subagent")
    assert state(agent) == "done"
    assert counts(agent) == ("1", "1 Explore")
    stop(agent, "x")
    assert (state(agent), counts(agent)) == ("done", ("0", ""))


# --- A4: how it looks (agents.conf formats) -------------------------------------------


def glyph(server: TmuxServer, agent: FakeAgent) -> str:
    return server.tmux("display", "-p", "-t", agent.pane, "#{E:@agent-glyph}")


@rule("A4")
@pytest.mark.parametrize("current", ["done", "idle", "ready"])
def test_A4_background_glyph_while_subagents_run(server: TmuxServer, current: str) -> None:
    agent = server.agent("claude")
    if current == "ready":  # a subagent before the first prompt
        agent.hook("SessionStart", source="startup")
        start(agent, "x")
    else:
        working(agent)
        start(agent, "x")
        # done: the turn ends; idle: a compaction with nothing to go back to
        agent.hook("Stop" if current == "done" else "PostCompact")
    assert state(agent) == current
    assert "◐" in glyph(server, agent)
    stop(agent, "x")
    assert "◐" not in glyph(server, agent)
