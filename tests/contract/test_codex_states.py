"""§6 Codex through its hooks: sessions and turns (X1), calls and waits
(X2), state (X3), question sent (X4) and Codex options (X6). Includes the
transition scenarios of agents/tests/test_codex.py as black-box tests."""

import json
import time
from collections.abc import Callable
from pathlib import Path

import pytest

from harness import procs
from harness.claude import shown, state
from harness.codex import Codex, Rollout
from harness.marks import rule
from harness.tmux import TmuxServer


@pytest.fixture
def codex(server: TmuxServer, tmp_path: Path) -> Codex:
    """A codex agent that is working on turn "one"."""
    c = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    c.start()
    assert state(c.agent) == "working"
    return c


def needs(c: Codex) -> tuple[str, str, str]:
    options = shown(c.agent)
    return options["@agent_state"], options.get("@agent_needs", ""), options.get("@agent_needs_id", "")


# --- X1 sessions and turns ------------------------------------------------------------


@rule("X1")
def test_X1_events_of_another_session_are_ignored(codex: Codex) -> None:
    before = shown(codex.agent)
    codex.hook("PreToolUse", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"},
               session_id="another-session")
    codex.hook("Stop", session_id="another-session")
    codex.hook("SessionEnd", session_id="another-session")
    codex.hook("SessionStart", source="compact", session_id="another-session")
    assert shown(codex.agent) == before


@rule("X1", "X3")
def test_X1_a_new_session_resets(codex: Codex) -> None:
    codex.ask("shell", command="unmatched")
    codex.agent.session_id = "second-session"
    codex.hook("SessionStart", source="startup")
    options = shown(codex.agent)
    assert (options["@agent_state"], options["@agent_session"]) == ("ready", "second-session")
    assert "@agent_needs" not in options
    codex.prompt("t2")
    assert state(codex.agent) == "working"


@rule("X1")
def test_X1_events_of_an_old_turn_are_ignored(codex: Codex) -> None:
    before = shown(codex.agent)
    codex.hook("PreToolUse", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"},
               turn_id="zero")
    codex.hook("PermissionRequest", tool_name="Bash", tool_input={"command": "ls"}, turn_id="zero")
    codex.hook("Stop", turn_id="zero")
    assert shown(codex.agent) == before


@rule("X1", "X2")
def test_X1_a_new_turn_forgets_calls_and_waits(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    codex.ask("Bash", command="ls")
    assert needs(codex)[0] == "needs"
    codex.prompt("two")
    assert needs(codex) == ("working", "", "")
    codex.post("Bash", "a")
    assert state(codex.agent) == "working"


# --- X2 calls and waits (transition scenarios of test_codex.py) -------------------------


@rule("X2", "X3")
def test_X2_parallel_permission_without_id(codex: Codex) -> None:
    codex.pre("Bash", "a", command="echo A")
    codex.pre("Bash", "b", command="echo B")
    codex.ask("Bash", command="echo A", description="approval")  # description is not compared
    codex.post("Bash", "b")
    assert needs(codex) == ("needs", "permission", "a")
    codex.post("Bash", "a")
    assert needs(codex) == ("working", "", "")


@rule("X2", "X3")
def test_X2_unmatched_permission_is_conservative(codex: Codex) -> None:
    codex.ask("Bash", command="missing pre")
    assert needs(codex)[:2] == ("needs", "permission")
    codex.post("Bash", "other")
    assert needs(codex)[0] == "needs"
    codex.hook("Interrupt")
    assert needs(codex) == ("idle", "", "")


@rule("X2")
def test_X2_identical_parallel_commands_wait_for_both(codex: Codex) -> None:
    codex.pre("Bash", "a", command="echo same")
    codex.pre("Bash", "b", command="echo same")
    codex.ask("Bash", command="echo same")
    codex.post("Bash", "a")
    assert state(codex.agent) == "needs"
    codex.post("Bash", "b")
    assert state(codex.agent) == "working"


@rule("X2", "X3")
def test_X2_question_and_other_tool_completion(codex: Codex) -> None:
    codex.pre("request_user_input", "q")
    assert needs(codex) == ("needs", "question", "q")
    codex.post("Bash", "other")
    assert needs(codex) == ("needs", "question", "q")
    codex.post("request_user_input", "q")
    assert needs(codex) == ("working", "", "")


@rule("X2")
def test_X2_fingerprint_is_tool_and_canonical_input(codex: Codex) -> None:
    codex.hook("PreToolUse", tool_name="Bash", tool_use_id="a", tool_input={"b": 1, "a": [2, 3]})
    codex.hook("PermissionRequest", tool_name="Bash", tool_input={"a": [2, 3], "b": 1})
    assert needs(codex)[2] == "a"
    codex.post("Bash", "a")
    codex.pre("Bash", "c", command="x")
    codex.ask("shell", command="x")  # another tool: no match, waits until the turn ends
    codex.post("Bash", "c")
    assert state(codex.agent) == "needs"


@rule("X2", "X3")
def test_X2_the_oldest_wait_shows(codex: Codex) -> None:
    codex.pre("request_user_input", "q")
    codex.pre("Bash", "a", command="rm x")
    codex.ask("Bash", command="rm x")
    assert needs(codex) == ("needs", "question", "q")
    codex.post("request_user_input", "q")
    assert needs(codex) == ("needs", "permission", "a")


# --- X3 state ------------------------------------------------------------------------------


@rule("X3")
def test_X3_late_result_after_stop_changes_nothing(codex: Codex) -> None:
    codex.hook("Stop", last_assistant_message="done")
    assert state(codex.agent) == "done"
    codex.post("Bash", "late")
    assert state(codex.agent) == "done"


@rule("X3", "H1b")
def test_X3_compaction_restart_is_unchanged(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    before = shown(codex.agent)
    codex.hook("SessionStart", source="compact")
    assert shown(codex.agent) == before


@rule("X3")
def test_X3_compaction(codex: Codex) -> None:
    codex.hook("PreCompact")
    assert state(codex.agent) == "compacting"
    codex.hook("PostCompact")
    assert state(codex.agent) == "working"


@rule("X3")
def test_X3_compaction_hides_an_open_wait(codex: Codex) -> None:
    codex.pre("request_user_input", "q")
    codex.hook("PreCompact")
    assert state(codex.agent) == "compacting"
    codex.hook("PostCompact")
    assert needs(codex) == ("needs", "question", "q")


@rule("X3")
def test_X3_stop_ends_the_turn_and_its_waits(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    codex.ask("Bash", command="ls")
    codex.hook("Stop", last_assistant_message="stopped")
    options = shown(codex.agent)
    assert (options["@agent_state"], options["@agent_msg"]) == ("done", "stopped")
    assert "@agent_needs" not in options and "@agent_tool" not in options


@rule("X3")
def test_X3_interrupt(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    codex.hook("PreCompact")
    codex.hook("Interrupt")
    options = shown(codex.agent)
    assert options["@agent_state"] == "idle"
    assert "@agent_tool" not in options and "@agent_prev" not in options


@rule("X3", "H9", "V2")
def test_X3_stop_in_the_pane_in_front_is_idle(make_server: Callable[..., TmuxServer], tmp_path: Path) -> None:
    server = make_server(focus="client")
    c = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    server.tmux("select-window", "-t", c.pane)
    c.start()
    c.hook("Stop")
    assert state(c.agent) == "idle"


@rule("X3", "H11", "P1")
def test_X3_session_end_clears(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    codex.hook("SessionEnd")
    time.sleep(3)  # past an observation tick
    assert codex.agent.options() == {}


@rule("H14", "X3")
def test_X3_identity(codex: Codex) -> None:
    options = shown(codex.agent)
    assert options["@agent"] == "codex"
    assert options["@agent_session"] == codex.agent.session_id
    assert options["@agent_transcript"] == str(codex.rollout.path)
    assert "@agent_profile" not in options  # Claude only


# --- X4 question sent ------------------------------------------------------------------


@rule("X4")
@pytest.mark.parametrize("response", [{"accepted": True}, json.dumps({"accepted": True})])
def test_X4_question_sent(codex: Codex, response: object) -> None:
    codex.post("request_user_input_async", "q", tool_response=response)
    assert shown(codex.agent)["@agent_question"] == "sent"
    assert state(codex.agent) == "working"  # an acknowledgement, not an answer
    codex.hook("Stop")
    assert shown(codex.agent)["@agent_question"] == "sent"
    codex.prompt("two")
    assert "@agent_question" not in shown(codex.agent)


@rule("X4")
@pytest.mark.parametrize("response", [{"accepted": False}, "{}", None])
def test_X4_not_sent_unless_accepted(codex: Codex, response: object) -> None:
    codex.post("request_user_input_async", "q", tool_response=response)
    assert "@agent_question" not in shown(codex.agent)


# --- X6 Codex options --------------------------------------------------------------------


@rule("X6")
def test_X6_turn_pid_and_outcome(codex: Codex) -> None:
    options = shown(codex.agent)
    assert options["@agent_turn"] == "one"
    assert options["@agent_pid"] == str(codex.agent.pid)
    assert options["@agent_pid_start"] == procs.start_time(codex.agent.pid)
    assert "@agent_outcome" not in options  # empty while it runs
    codex.hook("Stop")
    assert shown(codex.agent)["@agent_outcome"] == "complete"
    codex.prompt("two")
    options = shown(codex.agent)
    assert options["@agent_turn"] == "two" and "@agent_outcome" not in options
    codex.hook("Interrupt")
    assert shown(codex.agent)["@agent_outcome"] == "interrupted"


@rule("A1", "X6")
def test_X6_subagent_turn_does_not_replace_the_parent(codex: Codex) -> None:
    codex.hook("SubagentStart", agent_id="child", agent_type="worker", turn_id="child-turn")
    assert shown(codex.agent)["@agent_subs"] == "1"
    assert shown(codex.agent)["@agent_turn"] == "one"
    codex.hook("SubagentStop", agent_id="child", agent_type="worker", turn_id="child-turn")
    assert "@agent_subs" not in shown(codex.agent)
