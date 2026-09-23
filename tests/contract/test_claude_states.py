"""§3 Claude events → state (H1-H14), and clearing the pane (P1)."""

import time
from collections.abc import Callable

import pytest

from harness.agent import FakeAgent
from harness.claude import ask, near_now, next_second, post, pre, shown, state, working
from harness.marks import rule
from harness.tmux import TmuxServer

def sub(agent: FakeAgent, event: str, agent_id: str, agent_type: str = "Explore") -> None:
    agent.hook(event, agent_id=agent_id, agent_type=agent_type)


def assert_nothing(agent: FakeAgent, run: Callable[[], object]) -> None:
    before = agent.options()
    run()
    assert agent.options() == before


# --- H1 SessionStart ----------------------------------------------------------


@rule("H1")
@pytest.mark.parametrize("source", ["startup", "resume", "clear", None])
def test_H1_session_start_is_ready_and_forgets_tool_and_msg(server: TmuxServer, source: str | None) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="ls")
    agent.hook("SessionStart", source=source)
    assert state(agent) == "ready"
    assert "@agent_tool" not in shown(agent)
    agent.hook("UserPromptSubmit")
    agent.hook("Stop", last_assistant_message="finished")
    assert shown(agent)["@agent_msg"] == "finished"
    agent.hook("SessionStart", source=source)
    assert state(agent) == "ready"
    assert "@agent_msg" not in shown(agent)


@rule("H1", "A3")
def test_H1_forgets_the_old_sessions_subagents(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    sub(agent, "SubagentStart", "x")
    assert shown(agent)["@agent_subs"] == "1"
    agent.session_id = "second-session"
    agent.hook("SessionStart", source="clear")
    assert "@agent_subs" not in shown(agent)
    assert "@agent_subtypes" not in shown(agent)
    sub(agent, "SubagentStart", "y")
    assert shown(agent)["@agent_subs"] == "1"


@rule("H1", "A3")
def test_H1_forgets_the_new_sessions_subagents(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    sub(agent, "SubagentStart", "x")
    agent.hook("SessionStart", source="resume")  # same session id
    assert "@agent_subs" not in shown(agent)
    sub(agent, "SubagentStart", "y")
    assert shown(agent)["@agent_subs"] == "1"


@rule("H1b")
def test_H1b_compaction_restart_changes_nothing_of_the_turn(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="ls")
    sub(agent, "SubagentStart", "x")
    before = shown(agent)
    next_second()
    agent.hook("SessionStart", source="compact")
    assert shown(agent) == before


@rule("H1c")
@pytest.mark.parametrize("source", ["startup", "resume", "compact"])
def test_H1c_model(server: TmuxServer, source: str) -> None:
    agent = server.agent("claude")
    working(agent, model="first-model")
    assert shown(agent)["@agent_model"] == "first-model"
    agent.hook("SessionStart", source=source, model="second-model")
    assert shown(agent)["@agent_model"] == "second-model"
    agent.hook("SessionStart", source=source, model="")
    assert shown(agent)["@agent_model"] == "second-model"


# --- H2-H5 prompts and tools ----------------------------------------------------


@rule("H2")
def test_H2_prompt_is_working_and_forgets_tool_and_msg(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop", last_assistant_message="finished")
    agent.hook("UserPromptSubmit")
    assert state(agent) == "working"
    assert "@agent_msg" not in shown(agent)
    pre(agent, "Read", "a", file_path="/etc/hosts")
    agent.hook("UserPromptSubmit")
    assert "@agent_tool" not in shown(agent)


@rule("H3")
def test_H3_tool_call_is_working_with_its_label(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup")
    pre(agent, "Bash", "a", command="make")
    assert state(agent) == "working"
    assert shown(agent)["@agent_tool"] == "Bash: make"


@rule("H3", "H13")
def test_H3_tool_call_while_waiting_keeps_needs(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "AskUserQuestion", "q")
    since = agent.option("@agent_since")
    next_second()
    pre(agent, "Read", "r", file_path="notes.md")
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", "question")
    assert options["@agent_needs_id"] == "q"
    assert options["@agent_tool"] == "Read: notes.md"
    assert options["@agent_since"] == since


@rule("H4")
@pytest.mark.parametrize(
    ("tool", "kind"),
    [("AskUserQuestion", "question"), ("ExitPlanMode", "plan"), ("Bash", "permission"),
     ("mcp__server__tool", "permission")],
)
def test_H4_permission_request(server: TmuxServer, tool: str, kind: str) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, tool, "call-1", command="rm -r build")
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", kind)
    assert options["@agent_needs_id"] == "call-1"
    assert options["@agent_tool"] == f"{tool}: rm -r build"


@rule("H5", "H13")
@pytest.mark.parametrize("event", ["PostToolUse", "PostToolUseFailure"])
def test_H5_only_the_waiting_call_ends_the_wait(server: TmuxServer, event: str) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="echo A")
    pre(agent, "Bash", "b", command="echo B")
    ask(agent, "Bash", "a", command="echo A")
    post(agent, "Bash", "b", event)
    assert (state(agent), shown(agent)["@agent_needs_id"]) == ("needs", "a")
    agent.hook(event, tool_name="Bash")  # no tool id: not the waiting call either
    assert state(agent) == "needs"
    post(agent, "Bash", "a", event)
    options = shown(agent)
    assert options["@agent_state"] == "working"
    assert "@agent_needs" not in options and "@agent_needs_id" not in options


@rule("H5")
def test_H5_wait_without_call_id_ends_with_any_result(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Notification", notification_type="permission_prompt")
    assert state(agent) == "needs"
    post(agent, "Bash", "whatever")
    assert state(agent) == "working"


@rule("H5")
@pytest.mark.parametrize("event", ["PostToolUse", "PostToolUseFailure"])
def test_H5_result_is_working(server: TmuxServer, event: str) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup")
    post(agent, "Bash", "a", event)
    assert state(agent) == "working"


# --- H6-H7 notifications and elicitations ----------------------------------------


@rule("H6")
def test_H6_permission_prompt(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Notification", notification_type="permission_prompt")
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", "permission")


@rule("H6", "H13")
def test_H6_permission_prompt_keeps_an_open_wait(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "AskUserQuestion", "q")
    before = shown(agent)
    next_second()
    agent.hook("Notification", notification_type="permission_prompt")
    assert shown(agent) == before


@rule("H6b", "H13")
@pytest.mark.parametrize("kind", ["elicitation_dialog", "elicitation_url_dialog", "agent_needs_input"])
def test_H6b_input_notifications_are_questions(server: TmuxServer, kind: str) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Notification", notification_type=kind)
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", "question")
    ask(agent, "Bash", "a", command="ls")
    agent.hook("Notification", notification_type="permission_prompt")  # H6: unchanged
    since = agent.option("@agent_since")
    next_second()
    agent.hook("Notification", notification_type=kind)
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", "question")
    assert options["@agent_since"] == since


@rule("H6c", "H14")
@pytest.mark.parametrize("kind", ["idle_prompt", "auth_success", "", None])
def test_H6c_other_notifications_do_nothing_at_all(server: TmuxServer, kind: str | None) -> None:
    agent = server.agent("claude")
    working(agent)
    assert_nothing(
        agent,
        lambda: agent.hook(
            "Notification",
            notification_type=kind,
            session_id="another-session",
            permission_mode="plan",
            transcript_path="/tmp/other.jsonl",
        ),
    )


@rule("H7")
def test_H7_elicitation_is_a_question(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Elicitation")
    options = shown(agent)
    assert (options["@agent_state"], options["@agent_needs"]) == ("needs", "question")


@rule("H7b")
def test_H7b_elicitation_result_ends_a_wait(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Elicitation")
    agent.hook("ElicitationResult")
    assert state(agent) == "working"


@rule("H7b")
@pytest.mark.parametrize("current", ["ready", "done", "compacting"])
def test_H7b_elicitation_result_otherwise_changes_no_state(server: TmuxServer, current: str) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup")
    if current == "done":
        agent.hook("Stop")
    elif current == "compacting":
        agent.hook("PreCompact")
    assert state(agent) == current
    agent.hook("ElicitationResult")
    assert state(agent) == current


# --- H8 compaction ---------------------------------------------------------------


@rule("H8", "H8b")
@pytest.mark.parametrize("before", ["working", "needs", "done"])
def test_H8_compaction_returns_to_the_previous_state(server: TmuxServer, before: str) -> None:
    agent = server.agent("claude")
    working(agent)
    if before == "needs":
        ask(agent, "Bash", "a", command="ls")
    elif before == "done":
        agent.hook("Stop")
    agent.hook("PreCompact")
    assert (state(agent), shown(agent)["@agent_prev"]) == ("compacting", before)
    agent.hook("PreCompact")  # a second one keeps the state before the first
    assert shown(agent)["@agent_prev"] == before
    agent.hook("PostCompact")
    assert state(agent) == before


@rule("H8", "H13")
def test_H8_compacting_leaves_needs(server: TmuxServer) -> None:
    """H13: a state other than needs unsets the kind and the call id, and
    PostCompact gives none back."""
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a", command="ls")
    agent.hook("PreCompact")
    assert "@agent_needs" not in shown(agent) and "@agent_needs_id" not in shown(agent)
    agent.hook("PostCompact")
    options = shown(agent)
    assert options["@agent_state"] == "needs"
    assert "@agent_needs" not in options and "@agent_needs_id" not in options


@rule("H8b")
def test_H8b_compaction_end_without_start_is_idle(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup")
    agent.hook("PostCompact")
    assert state(agent) == "idle"


# --- H9-H10 end of turn ----------------------------------------------------------


@rule("H9")
def test_H9_stop_is_done_with_the_reply(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="ls")
    agent.hook("Stop", last_assistant_message="All tests pass.")
    options = shown(agent)
    assert options["@agent_state"] == "done"
    assert options["@agent_msg"] == "All tests pass."
    assert "@agent_tool" not in options


@rule("H9")
def test_H9_stop_without_reply_clears_the_message(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop", last_assistant_message="first")
    agent.hook("Stop")
    assert state(agent) == "done"
    assert "@agent_msg" not in shown(agent)


@rule("H9", "V2")
def test_H9_stop_in_the_pane_in_front_is_idle(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    working(agent)
    agent.hook("Stop", last_assistant_message="finished")
    assert state(agent) == "idle"
    assert shown(agent)["@agent_msg"] == "finished"


@rule("H9", "V2")
def test_H9_stop_behind_another_window_is_done(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    agent = server.agent("claude")  # a window of the client's session, not in front
    working(agent)
    agent.hook("Stop", last_assistant_message="finished")
    assert state(agent) == "done"


@rule("H10")
@pytest.mark.parametrize(
    ("fields", "message"),
    [
        ({"error": "rate limited", "last_assistant_message": "partial"}, "rate limited"),
        ({"last_assistant_message": "partial"}, "partial"),
        ({}, "unknown error"),
    ],
)
def test_H10_stop_failure_is_error(server: TmuxServer, fields: dict[str, str], message: str) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("StopFailure", **fields)
    options = shown(agent)
    assert options["@agent_state"] == "error"
    assert options["@agent_msg"] == message


# --- H11 SessionEnd, P1 --------------------------------------------------------------


@rule("H11", "P1")
def test_H11_session_end_clears_the_pane(server: TmuxServer) -> None:
    agent = server.agent("claude", env={"CLAUDE_CONFIG_DIR": "/nonexistent/profiles/work"})
    working(agent, model="m", transcript_path="/tmp/t.jsonl", permission_mode="plan")
    sub(agent, "SubagentStart", "x")
    ask(agent, "Bash", "a", command="ls")
    server.sink.wait_for(lambda line: line["effect"] == "notify")  # effects settled
    agent.hook("PreCompact")
    assert len(shown(agent)) >= 10
    agent.hook("SessionEnd")
    assert agent.options() == {}  # internal options too


@rule("H11", "P1", "N3")
def test_H11_session_end_right_after_needs(server: TmuxServer) -> None:
    """SessionEnd before the needs notification went out: once things settle,
    no notification is left open and the pane has no agent option left."""
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a", command="ls")
    agent.hook("SessionEnd")
    time.sleep(2)
    effects = [line["effect"] for line in server.sink.lines() if line.get("pane") == agent.pane]
    assert effects in ([], ["notify", "notify-close"]), effects
    assert agent.options() == {}


@rule("H11", "A3")
def test_H11_session_end_forgets_the_subagents(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    sub(agent, "SubagentStart", "x")
    agent.hook("SessionEnd")
    sub(agent, "SubagentStart", "y")
    assert shown(agent)["@agent_subs"] == "1"


# --- H12 other events ------------------------------------------------------------------


@rule("H12")
@pytest.mark.parametrize(
    "event", ["Interrupt", "CodexReconcile", "SubagentStart", "SubagentStop", "Setup", "Bogus", ""]
)
def test_H12_other_events_do_nothing_at_all(server: TmuxServer, event: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert_nothing(
        agent,
        lambda: agent.hook(event, session_id="another-session", permission_mode="plan",
                           last_assistant_message="x"),
    )


# --- H13 writing the state ---------------------------------------------------------------


@rule("H13")
def test_H13_since_is_the_last_change(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    first = agent.option("@agent_since")
    assert near_now(first)
    next_second()
    pre(agent, "Bash", "a", command="ls")  # working again: no change
    agent.hook("UserPromptSubmit")
    assert agent.option("@agent_since") == first
    ask(agent, "Bash", "a", command="ls")
    second = agent.option("@agent_since")
    assert near_now(second) and int(second) > int(first)


# --- H14 identity ---------------------------------------------------------------------------


@rule("H14")
def test_H14_identity_without_session_start(server: TmuxServer) -> None:
    """Hooks installed while a session already runs: the first event names it."""
    agent = server.agent("claude")
    agent.hook("PreToolUse", tool_name="Bash", tool_input={"command": "ls"},
               transcript_path="/tmp/s.jsonl", permission_mode="acceptEdits")
    options = shown(agent)
    assert options["@agent"] == "claude"
    assert options["@agent_session"] == agent.session_id
    assert options["@agent_transcript"] == "/tmp/s.jsonl"
    assert options["@agent_mode"] == "acceptEdits"
    assert options["@agent_profile"] == "default"


@rule("H14")
def test_H14_only_non_empty_values_replace(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent, transcript_path="/tmp/s.jsonl", permission_mode="plan")
    agent.hook("UserPromptSubmit", transcript_path="", permission_mode="", session_id="")
    options = shown(agent)
    assert options["@agent_transcript"] == "/tmp/s.jsonl"
    assert options["@agent_mode"] == "plan"
    assert options["@agent_session"] == agent.session_id
    agent.hook("UserPromptSubmit", session_id="renamed", permission_mode="default")
    options = shown(agent)
    assert (options["@agent_session"], options["@agent_mode"]) == ("renamed", "default")


@rule("H14")
@pytest.mark.parametrize(
    ("config_dir", "profile"),
    [
        (None, "default"),
        ("/nonexistent/.claude", "default"),
        ("/nonexistent/.claude/", "default"),
        ("/nonexistent/profiles/work", "work"),
        ("/nonexistent/profiles/work/", "work"),
    ],
)
def test_H14_profile(server: TmuxServer, config_dir: str | None, profile: str) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup", _env={"CLAUDE_CONFIG_DIR": config_dir})
    assert shown(agent)["@agent_profile"] == profile


@rule("H14", "O2")
def test_H14_follows_the_latest_event(server: TmuxServer) -> None:
    agent = server.agent("claude")
    agent.hook("SessionStart", source="startup", _env={"CLAUDE_CONFIG_DIR": "/nonexistent/work"})
    assert shown(agent)["@agent_profile"] == "work"
    agent.hook("UserPromptSubmit", _env={"CLAUDE_CONFIG_DIR": "/nonexistent/home"})
    assert shown(agent)["@agent_profile"] == "home"
