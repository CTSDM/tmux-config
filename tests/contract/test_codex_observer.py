"""§6 Codex beyond its hooks: the rollout observation (X5), background
commands (X7), privacy of the bookkeeping (X8) and reconcile (E2). Includes
the rollout and integration scenarios of agents/tests/test_codex.py as
black-box tests."""

import signal
import time
from pathlib import Path

import pytest

from harness import procs
from harness.claude import shown, state
from harness.codex import Codex, Rollout
from harness.marks import rule
from harness.tmux import TmuxServer
from harness.wait import eventually

TICK = 6.0  # "about every 2 s", with margin


@pytest.fixture
def codex(server: TmuxServer, tmp_path: Path) -> Codex:
    """A codex agent that is working on turn "one"."""
    c = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    c.start()
    return c


def wait_state(c: Codex, expected: str, timeout: float = TICK) -> None:
    eventually(lambda: state(c.agent), lambda s: s == expected, timeout, f"{c.pane} {expected}")


def stays(c: Codex, expected: str, seconds: float = 4.5) -> None:
    """Several observation ticks go by without a change."""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        assert state(c.agent) == expected
        time.sleep(0.2)


# --- X5 what the rollout says ---------------------------------------------------------


@rule("X5", "X6")
def test_X5_task_complete_is_done(server: TmuxServer, codex: Codex) -> None:
    codex.rollout.complete("one", message="All done.")
    wait_state(codex, "done")
    options = shown(codex.agent)
    assert (options["@agent_msg"], options["@agent_outcome"]) == ("All done.", "complete")
    server.sink.wait_for(lambda line: line["effect"] == "notify" and line["body"] == "All done.")


@rule("X5", "X6", "S4")
def test_X5_error_after_many_records(server: TmuxServer, codex: Codex) -> None:
    """test_codex.py: an error followed by more than 300 records."""
    codex.rollout.complete("one", error={"message": "limit", "codex_error_info": "usage_limit_exceeded"})
    codex.rollout.append(*["null\n"] * 350)
    wait_state(codex, "error")
    options = shown(codex.agent)
    assert (options["@agent_msg"], options["@agent_outcome"]) == ("limit", "error")
    server.sink.wait_sound("oh-man")
    note = server.sink.wait_for(lambda line: line["effect"] == "notify")
    assert (note["urgency"], note["body"]) == ("critical", "Stopped: limit")


@rule("X5", "X6")
def test_X5_turn_aborted_is_idle(codex: Codex) -> None:
    codex.pre("Bash", "a", command="ls")
    codex.rollout.aborted("one")
    wait_state(codex, "idle")
    options = shown(codex.agent)
    assert options["@agent_outcome"] == "interrupted" and "@agent_tool" not in options


@rule("X5")
def test_X5_old_abort_cannot_finish_a_new_turn(codex: Codex) -> None:
    codex.prompt("new")
    codex.rollout.aborted("one")
    stays(codex, "working")


@rule("X5")
def test_X5_output_closes_a_call_and_its_wait(codex: Codex) -> None:
    codex.pre("Bash", "a", command="make")
    codex.ask("Bash", command="make")
    assert state(codex.agent) == "needs"
    codex.rollout.output("a")
    wait_state(codex, "working")


@rule("X5", "P2")
def test_X5_needs_without_a_wait_is_working(server: TmuxServer, codex: Codex) -> None:
    server.tmux("set", "-p", "-t", codex.pane, "@agent_state", "needs")
    wait_state(codex, "working")


@rule("X5")
def test_X5_partial_line_then_rotation(codex: Codex) -> None:
    """test_codex.py: never consume an unfinished last line; restart on a new file."""
    end = '{"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "one"}}\n'
    codex.rollout.append(end[:15])
    stays(codex, "working")
    codex.rollout.append(end[15:])
    wait_state(codex, "done")
    codex.prompt("two")
    codex.rollout.replace({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "two"}})
    stays(codex, "working")
    codex.rollout.complete("two")
    wait_state(codex, "done")


@rule("X5")
def test_X5_missing_rollout_is_not_completion(server: TmuxServer, tmp_path: Path) -> None:
    c = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    c.rollout.path.unlink()
    c.start()
    stays(c, "working")


@rule("X5", "X6")
def test_X5_collaboration_mode(server: TmuxServer, tmp_path: Path) -> None:
    c = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    c.hook("SessionStart", source="startup")
    c.prompt("one", mode="plan")
    eventually(lambda: shown(c.agent).get("@agent_collaboration"), lambda v: v == "plan", TICK,
               "collaboration mode")
    c.post("request_user_input_async", "q", tool_response='{"accepted":true}')
    label = server.tmux("display", "-p", "-t", c.pane, "#{E:@agent-label}")
    assert label == "working · plan mode · question sent"
    c.prompt("two")
    eventually(lambda: shown(c.agent).get("@agent_collaboration"), lambda v: v is None, TICK,
               "no collaboration mode in turn two")


@rule("X5", "H11", "P1")
def test_X5_agent_gone_clears(codex: Codex) -> None:
    codex.agent.exit()
    eventually(lambda: shown(codex.agent), lambda o: o == {}, TICK, "cleared")


@rule("X5", "E2")
def test_X5_observation_gives_up_then_reconcile_observes(codex: Codex) -> None:
    """10 s after a Stop the rollout never confirmed, observation stops; a
    reconcile makes one observation."""
    codex.hook("Stop")
    time.sleep(10 + 2 * 2 + 2)  # 10 s, checked on a 2 s tick, and margin
    codex.rollout.complete("one", error={"message": "late error"})
    stays(codex, "done")
    codex.agent.server.reconcile(codex.pane)
    wait_state(codex, "error", timeout=3)
    assert shown(codex.agent)["@agent_msg"] == "late error"


@rule("E2")
def test_E2_codex_agent_gone(server: TmuxServer, codex: Codex) -> None:
    codex.agent.exit()
    server.reconcile(codex.pane)
    eventually(lambda: shown(codex.agent), lambda o: o == {}, 3, "cleared")


# --- X7 background commands --------------------------------------------------------------


def command(c: Codex, script: str, *, thread: str | None = None, new_session: bool = True) -> int:
    """A command tree as Codex starts it: its own Unix session, and the
    session's CODEX_THREAD_ID in its environment."""
    env: dict[str, str | None] = {"CODEX_THREAD_ID": thread or c.agent.session_id}
    return c.agent.spawn(["/bin/sh", "-c", script], env=env, new_session=new_session)


def bg(c: Codex) -> str:
    return shown(c.agent).get("@agent_bg", "")


def wait_bg(c: Codex, value: str, timeout: float = TICK) -> None:
    eventually(lambda: bg(c), lambda v: v == value, timeout, f"@agent_bg == {value!r}")


@rule("X7")
def test_X7_command_left_running_at_stop(codex: Codex) -> None:
    """test_codex.py, turn four: an MCP-like child (same Unix session) does
    not count; a command tree does until it ends."""
    mcp = command(codex, "sleep 300; exit 0", new_session=False)
    tree = command(codex, "sleep 300 & wait")
    codex.hook("Stop")
    codex.rollout.complete("one")
    wait_bg(codex, "1")
    assert state(codex.agent) == "done"
    codex.agent.signal(tree, signal.SIGTERM, group=True)
    wait_bg(codex, "")
    assert procs.alive(mcp)


@rule("X7")
def test_X7_tree_outlives_its_leader(codex: Codex) -> None:
    """test_codex.py, turn five: the leader exits, its reparented child keeps
    the tree counting."""
    tree = command(codex, "sleep 300 & sleep 2")
    codex.hook("Stop")
    codex.rollout.complete("one")
    wait_bg(codex, "1")
    assert codex.agent.wait(tree, timeout=4) == 0
    stays_bg = time.monotonic() + 4.5
    while time.monotonic() < stays_bg:
        assert bg(codex) == "1"
        time.sleep(0.2)
    codex.agent.signal(tree, signal.SIGTERM, group=True)
    wait_bg(codex, "")


@rule("X7")
def test_X7_nested_trees_count_once(codex: Codex) -> None:
    command(codex, "setsid sh -c 'sleep 300' & sleep 300 & wait")
    command(codex, "sleep 300 & wait")
    codex.hook("Stop")
    wait_bg(codex, "2")


@rule("X7")
@pytest.mark.parametrize("kind", ["other thread", "code mode host"])
def test_X7_what_does_not_count(codex: Codex, tmp_path: Path, kind: str) -> None:
    if kind == "other thread":
        command(codex, "sleep 300 & wait", thread="another-thread")
    else:
        host = tmp_path / "codex-code-mode-host"
        host.symlink_to("/bin/sleep")
        codex.agent.spawn([str(host), "300"], env={"CODEX_THREAD_ID": codex.agent.session_id},
                          new_session=True)
    codex.hook("Stop")
    time.sleep(3)
    assert bg(codex) == ""


@rule("X7")
@pytest.mark.parametrize("event", ["Interrupt", "Stop"])
def test_X7_recounted_at_the_end_of_a_turn(codex: Codex, event: str) -> None:
    command(codex, "sleep 300 & wait")
    codex.hook(event)
    wait_bg(codex, "1", timeout=3)


# --- X8 privacy --------------------------------------------------------------------------


@rule("X8")
def test_X8_bookkeeping_keeps_no_content(server: TmuxServer, codex: Codex) -> None:
    canary = "canary-words-of-content"
    codex.hook("UserPromptSubmit", prompt=f"prompt {canary}")
    codex.pre("Bash", "a", command=f"echo command {canary}")
    codex.ask("Bash", command=f"echo command {canary}", description=f"why {canary}")
    codex.post("Bash", "a", tool_response=f"output {canary}")
    codex.hook("Stop", last_assistant_message=f"reply {canary}")
    time.sleep(3)
    leaks: list[str] = []
    for top in ("run", "state"):
        for path in (server.root / top).rglob("*"):
            if path.is_file() and not path.is_socket():
                try:
                    if canary in path.read_text(errors="replace"):
                        leaks.append(str(path.relative_to(server.root)))
                except OSError:
                    pass
    assert leaks == []
