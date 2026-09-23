"""§11 Notifications (N1-N3) and reminders (N4, CHANGE C1)."""

import socket
import time
from collections.abc import Callable
from typing import Any

import pytest

from harness.agent import FakeAgent
from harness.claude import ask, next_second, post, pre, working
from harness.impl import IMPL
from harness.marks import change, rule
from harness.tmux import TmuxServer

SETTLE = 1.0  # notifications arrive after the hook returns
REMIND = 3  # @agent_remind_after for these tests: past the 2.5 s sound debounce

type Line = dict[str, Any]


def pane_lines(server: TmuxServer, agent: FakeAgent, after: int = 0) -> list[Line]:
    return [line for line in server.sink.since(after) if line.get("pane") == agent.pane]


def notes_after(server: TmuxServer, agent: FakeAgent, run: Callable[[], object]) -> list[Line]:
    mark = server.sink.mark()
    run()
    time.sleep(SETTLE)
    return [line for line in pane_lines(server, agent, mark) if line["effect"] == "notify"]


def one_note(server: TmuxServer, agent: FakeAgent, run: Callable[[], object]) -> tuple[str, str, str]:
    notes = notes_after(server, agent, run)
    assert len(notes) == 1, notes
    return notes[0]["urgency"], notes[0]["title"], notes[0]["body"]


# --- N1 when --------------------------------------------------------------------


@rule("N1", "N2", "O1")
def test_N1_on_needs_done_and_error(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert one_note(server, agent, lambda: ask(agent, "Bash", "a", command="ls")) == (
        "critical", "main", "Needs permission: Bash: ls")
    post(agent, "Bash", "a")
    assert one_note(server, agent, lambda: agent.hook("Stop", last_assistant_message="Done.")) == (
        "normal", "main", "Done.")
    agent.hook("UserPromptSubmit")
    assert one_note(server, agent, lambda: agent.hook("StopFailure", error="overloaded")) == (
        "critical", "main", "Stopped: overloaded")


@rule("N1")
def test_N1_not_again_while_needs(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a")
    time.sleep(SETTLE)
    assert notes_after(server, agent, lambda: ask(agent, "Bash", "b")) == []
    assert notes_after(server, agent, lambda: agent.hook("Elicitation")) == []


@rule("N1")
def test_N1_not_for_other_states(server: TmuxServer) -> None:
    agent = server.agent("claude")

    def others() -> None:
        agent.hook("SessionStart", source="startup")
        agent.hook("UserPromptSubmit")
        pre(agent, "Bash", "a")
        agent.hook("PreCompact")
        agent.hook("PostCompact")

    assert notes_after(server, agent, others) == []


@rule("N1")
def test_N1_not_when_muted(server: TmuxServer) -> None:
    server.set_global("@agent_mute_test", "on")
    agent = server.agent("claude")
    working(agent)

    def alerts() -> None:
        ask(agent, "Bash", "a")
        agent.hook("Stop")
        agent.hook("UserPromptSubmit")
        agent.hook("StopFailure", error="x")

    assert notes_after(server, agent, alerts) == []


@rule("N1", "A4")
def test_N1_not_done_while_subagents_run(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("SubagentStart", agent_id="s", agent_type="Explore")
    assert notes_after(server, agent, lambda: agent.hook("Stop")) == []
    agent.hook("UserPromptSubmit")
    assert one_note(server, agent, lambda: agent.hook("StopFailure", error="x"))[0] == "critical"


# --- N2 content -----------------------------------------------------------------


@rule("N2")
@pytest.mark.parametrize(
    ("title", "shown"),
    [
        ("✳ Fix CSV export", "main · Fix CSV export"),
        ("Fix CSV export | tmux-ricing", "main · Fix CSV export"),
        ("a | b | c", "main · a | b"),
        ("✳ x | y", "main · x"),
        ("plain", "main · plain"),
        (None, "main"),  # the host name, tmux's default title
    ],
)
def test_N2_title(server: TmuxServer, title: str | None, shown: str) -> None:
    agent = server.agent("claude")
    server.tmux("select-pane", "-t", agent.pane, "-T", title or socket.gethostname())
    working(agent)
    assert one_note(server, agent, lambda: agent.hook("Stop"))[1] == shown


@rule("N2")
@pytest.mark.parametrize(
    ("tool", "body"),
    [("AskUserQuestion", "Has a question for you"), ("ExitPlanMode", "Plan ready for your review")],
)
def test_N2_question_and_plan(server: TmuxServer, tool: str, body: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert one_note(server, agent, lambda: ask(agent, tool, "a")) == ("critical", "main", body)


@rule("N2", "H6b")
def test_N2_input_notification_is_a_question(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    note = one_note(server, agent, lambda: agent.hook("Notification", notification_type="elicitation_dialog"))
    assert note == ("critical", "main", "Has a question for you")


@rule("N2")
def test_N2_permission_names_this_request(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Read", "r", file_path="a.txt")
    note = one_note(server, agent, lambda: ask(agent, "Bash", "a", command="echo #1"))
    assert note[2] == "Needs permission: Bash: echo #1"


@rule("N2", "H6")
def test_N2_permission_prompt_names_the_last_tool_unescaped(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="echo #1")
    note = one_note(server, agent, lambda: agent.hook("Notification", notification_type="permission_prompt"))
    assert note[2] == "Needs permission: Bash: echo #1"


@rule("N2", "H6", "P2")
def test_P2_N2_permission_prompt_names_a_tool_set_from_outside(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    server.tmux("set", "-p", "-t", agent.pane, "@agent_tool", "Bash: make ##2")
    note = one_note(server, agent, lambda: agent.hook("Notification", notification_type="permission_prompt"))
    assert note[2] == "Needs permission: Bash: make #2"


@rule("N2", "H6")
def test_N2_permission_prompt_without_tool(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    note = one_note(server, agent, lambda: agent.hook("Notification", notification_type="permission_prompt"))
    assert note[2] == "Needs permission"


@rule("N2")
def test_N2_done_without_reply(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert one_note(server, agent, lambda: agent.hook("Stop"))[2] == "Finished"


@rule("N2", "H10")
def test_N2_error_without_error_text(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    note = one_note(server, agent, lambda: agent.hook("StopFailure", last_assistant_message="partial"))
    assert note == ("critical", "main", "Stopped: error")


# --- N3 one per pane -------------------------------------------------------------


@rule("N3")
def test_N3_leaving_needs_closes_it(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a")
    time.sleep(SETTLE)
    mark = server.sink.mark()
    post(agent, "Bash", "a")
    server.sink.wait_for(lambda line: line["effect"] == "notify-close" and line["pane"] == agent.pane,
                         after=mark)


@rule("N3", "N1")
def test_N3_needs_then_done(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a")
    time.sleep(SETTLE)
    mark = server.sink.mark()
    agent.hook("Stop", last_assistant_message="gave up")
    time.sleep(SETTLE)
    lines = pane_lines(server, agent, mark)
    assert [line["effect"] for line in lines] == ["notify-close", "notify"]
    assert lines[1]["body"] == "gave up"


@rule("N3", "H11")
def test_N3_session_end_closes_it(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("Stop")
    time.sleep(SETTLE)
    mark = server.sink.mark()
    agent.hook("SessionEnd")
    server.sink.wait_for(lambda line: line["effect"] == "notify-close" and line["pane"] == agent.pane,
                         after=mark)


@rule("N3")
def test_N3_notifications_are_per_pane(server: TmuxServer) -> None:
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    ask(a, "Bash", "x")
    ask(b, "Bash", "y")
    time.sleep(SETTLE)
    mark = server.sink.mark()
    post(a, "Bash", "x")
    time.sleep(SETTLE)
    assert [line["effect"] for line in pane_lines(server, a, mark)] == ["notify-close"]
    assert pane_lines(server, b, mark) == []


# --- N4 reminders ----------------------------------------------------------------


def arm(agent: FakeAgent, call: str = "a") -> None:
    """Enter needs just after a second boundary, clear of the C2 race."""
    next_second()
    ask(agent, "Bash", call)


def reminders(server: TmuxServer, agent: FakeAgent) -> list[Line]:
    return [line for line in pane_lines(server, agent)
            if line["effect"] == "notify" and line["body"].startswith("Still waiting")]


@rule("N4", "S7")
def test_N4_reminder(server: TmuxServer) -> None:
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    server.tmux("select-pane", "-t", agent.pane, "-T", "✳ Fix CSV")
    working(agent)
    arm(agent)
    server.sink.wait_sound("come-to-papa", timeout=REMIND + 3)
    note = server.sink.wait_for(lambda line: line in reminders(server, agent), timeout=2)
    assert (note["urgency"], note["title"], note["body"]) == (
        "critical", "main · Fix CSV", "Still waiting for you, 0 min now")


@rule("N4")
def test_N4_no_reminder_once_answered(server: TmuxServer) -> None:
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    working(agent)
    arm(agent)
    post(agent, "Bash", "a")
    time.sleep(REMIND + 2)
    assert "come-to-papa" not in server.sink.sounds()
    assert reminders(server, agent) == []


@rule("N4")
def test_N4_only_the_latest_wait_reminds(server: TmuxServer) -> None:
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    working(agent)
    arm(agent)
    post(agent, "Bash", "a")
    arm(agent, "b")  # the next second: another @agent_since
    time.sleep(REMIND + 3)
    assert len(reminders(server, agent)) == 1


@rule("N4")
def test_N4_no_reminder_when_muted(server: TmuxServer) -> None:
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    working(agent)
    arm(agent)
    server.set_global("@agent_mute_test", "on")
    time.sleep(REMIND + 2)
    assert "come-to-papa" not in server.sink.sounds()
    assert reminders(server, agent) == []


@rule("N4", "V2")
def test_N4_no_reminder_when_visible(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    working(agent)
    arm(agent)  # its session is on screen: no notification yet
    server.tmux("select-window", "-t", agent.pane)  # now in front
    time.sleep(REMIND + 2)
    assert "come-to-papa" not in server.sink.sounds()
    assert reminders(server, agent) == []


@rule("N4", "V2")
def test_N4_reminds_when_only_the_session_is_on_screen(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(focus="client")
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")  # a background window of the client's session
    working(agent)
    arm(agent)
    server.sink.wait_sound("come-to-papa", timeout=REMIND + 3)
    server.sink.wait_for(lambda line: line in reminders(server, agent), timeout=2)
    first = [line for line in pane_lines(server, agent) if line["effect"] == "notify"]
    assert first == reminders(server, agent)  # N1 sent nothing: not away


@rule("N4", "C1")
@change("C1")
def test_C1_one_reminder_per_pane(server: TmuxServer) -> None:
    """Two waits within one second share @agent_since: only the reminder
    armed last may fire, so there is one."""
    server.set_global("@agent_remind_after", str(REMIND))
    agent = server.agent("claude")
    working(agent)
    next_second()
    ask(agent, "Bash", "a")
    post(agent, "Bash", "a")
    ask(agent, "Bash", "b")
    assert agent.option("@agent_state") == "needs"
    time.sleep(REMIND + 3)
    assert len(reminders(server, agent)) == 1


@rule("N4", "C2")
@pytest.mark.xfail(IMPL.name == "bash", reason="C2: bash can miss at a second boundary", strict=False)
def test_C2_reminder_despite_a_second_boundary(server: TmuxServer) -> None:
    """Needs entered at 5, 10 ... 50 ms before a second boundary: every pane
    gets its reminder (bash misses when the boundary falls between its two
    clock reads)."""
    server.set_global("@agent_remind_after", str(REMIND))
    agents = [server.agent("claude") for _ in range(10)]
    for agent in agents:
        working(agent)
    for i, agent in enumerate(agents):
        before_boundary = (1 - time.time() % 1) - 0.005 * (i + 1)
        time.sleep(before_boundary if before_boundary > 0 else before_boundary + 1)
        ask(agent, "Bash", "a")
    time.sleep(REMIND + 3)
    assert [len(reminders(server, agent)) for agent in agents] == [1] * 10


# --- N4 reading @agent_remind_after (CHANGE C7) ---------------------------------------


@rule("N4")
@pytest.mark.parametrize("value", ["3s", "0.05m", "3.0"])
def test_N4_remind_after_reads_like_sleep(server: TmuxServer, value: str) -> None:
    server.set_global("@agent_remind_after", value)
    agent = server.agent("claude")
    working(agent)
    arm(agent)
    time.sleep(2)
    assert reminders(server, agent) == []
    server.sink.wait_for(lambda line: line in reminders(server, agent), timeout=4)


@rule("N4", "C7")
@pytest.mark.parametrize(
    "value", ["", pytest.param("soon", marks=change("C7")), pytest.param("-5", marks=change("C7"))]
)
def test_C7_unreadable_remind_after_is_900(server: TmuxServer, value: str) -> None:
    server.set_global("@agent_remind_after", value)
    agent = server.agent("claude")
    working(agent)
    arm(agent)
    time.sleep(4)
    assert reminders(server, agent) == []
