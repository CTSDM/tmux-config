"""§8 Sounds (S1-S9) and rounds (R), as they reach the sink.

Every test has its own server, so its own 2.5 s debounce (S9)."""

import time
from collections.abc import Callable
from pathlib import Path

import pytest

from harness.agent import FakeAgent
from harness.claude import ask, near_now, post, pre, shown, working
from harness.marks import rule
from harness.tmux import TmuxServer

QUIET = 1.0  # how long "no sound" is watched
DEBOUNCE = 2.6  # past S9's 2.5 s window


def sounds_after(server: TmuxServer, run: Callable[[], object], wait: float = QUIET) -> list[str]:
    mark = server.sink.mark()
    run()
    time.sleep(wait)
    return [line["name"] for line in server.sink.since(mark) if line["effect"] == "sound"]


def visible(make_server: Callable[..., TmuxServer]) -> tuple[TmuxServer, FakeAgent]:
    server = make_server(focus="client")
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    return server, agent


# --- S1 entering needs ------------------------------------------------------------


@rule("S1")
@pytest.mark.parametrize(
    ("tool", "sound"),
    [("AskUserQuestion", "report-in"), ("ExitPlanMode", "wait-for-my-go"), ("Bash", "need-backup")],
)
def test_S1_permission_request(server: TmuxServer, tool: str, sound: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, tool, "a")) == [sound]


@rule("S1")
@pytest.mark.parametrize(
    ("event", "fields", "sound"),
    [
        ("Notification", {"notification_type": "permission_prompt"}, "need-backup"),
        ("Notification", {"notification_type": "elicitation_dialog"}, "report-in"),
        ("Elicitation", {}, "report-in"),
    ],
)
def test_S1_other_ways_into_needs(server: TmuxServer, event: str, fields: dict[str, str], sound: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: agent.hook(event, **fields)) == [sound]


@rule("S1")
def test_S1_only_when_entering_needs(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a", command="ls")
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: ask(agent, "Bash", "b", command="pwd")) == []
    assert sounds_after(server, lambda: agent.hook("Elicitation")) == []


@rule("S1", "V2")
def test_S1_not_when_visible(make_server: Callable[..., TmuxServer]) -> None:
    server, agent = visible(make_server)
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == []


# --- S2, S3 and rounds -----------------------------------------------------------------


@rule("S3", "R")
def test_S3_single_agent_round(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: agent.hook("Stop")) == ["enemy-down"]


@rule("S2", "S3", "R")
def test_S2_last_busy_agent_of_a_round_of_two_wins(server: TmuxServer) -> None:
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    assert sounds_after(server, lambda: a.hook("Stop")) == ["enemy-down"]  # b still works
    assert sounds_after(server, lambda: b.hook("Stop")) == ["ct-win"]  # outranks enemy-down


@rule("S2", "R")
def test_R_a_won_round_is_forgotten(server: TmuxServer) -> None:
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    a.hook("Stop")
    b.hook("Stop")
    time.sleep(DEBOUNCE)
    a.hook("UserPromptSubmit")
    assert sounds_after(server, lambda: a.hook("Stop")) == ["enemy-down"]


@rule("S2", "R")
@pytest.mark.parametrize("busy", ["needs", "compacting", "subagents"])
def test_R_round_goes_on_while_another_pane_is_busy(server: TmuxServer, busy: str) -> None:
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    if busy == "needs":
        ask(b, "Bash", "x")
    elif busy == "compacting":
        b.hook("PreCompact")
    else:
        b.hook("SubagentStart", agent_id="s", agent_type="Explore")
        b.hook("Stop")  # done, but its subagent still runs
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: a.hook("Stop")) == ["enemy-down"]


@rule("R")
def test_R_rounds_are_per_space(server: TmuxServer) -> None:
    server.session("other-space", space="other")
    a, b = server.agent("claude"), server.agent("claude", "other-space")
    working(a)
    working(b)
    assert sounds_after(server, lambda: a.hook("Stop")) == ["enemy-down"]  # b is not in a's round
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: b.hook("Stop")) == ["enemy-down"]


@rule("S2", "S3", "A4")
def test_S3_no_sound_while_own_subagents_run(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    agent.hook("SubagentStart", agent_id="s", agent_type="Explore")
    assert sounds_after(server, lambda: agent.hook("Stop")) == []


@rule("S2", "R")
def test_S2_plays_when_visible(make_server: Callable[..., TmuxServer]) -> None:
    server, a = visible(make_server)
    b = server.agent("claude")
    working(a)
    working(b)
    b.hook("Stop")
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: a.hook("Stop")) == ["ct-win"]


# --- S4, S5 --------------------------------------------------------------------------


@rule("S4")
def test_S4_stop_failure(make_server: Callable[..., TmuxServer]) -> None:
    server, agent = visible(make_server)  # plays even in front
    working(agent)
    assert sounds_after(server, lambda: agent.hook("StopFailure", error="x")) == ["oh-man"]


@rule("S5")
def test_S5_accepted_plan(make_server: Callable[..., TmuxServer]) -> None:
    server, agent = visible(make_server)  # plays even in front; no S1 sound first
    working(agent)
    ask(agent, "ExitPlanMode", "p")
    assert sounds_after(server, lambda: post(agent, "ExitPlanMode", "p")) == ["lets-do-this"]


@rule("S5")
def test_S5_only_for_the_plan_tool(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: post(agent, "Bash", "a")) == []


# --- S6, S8 test runs ------------------------------------------------------------------


@rule("S6", "S8")
@pytest.mark.parametrize(
    "command",
    ["npm test", "pnpm run test", "npx vitest", "pytest -x", "cd a && pytest", "go test ./...",
     "cargo nextest run", "make check", "python3 -m unittest", "(pytest)", "uv run pytest",
     "pytest\n-q"],
)
def test_S6_test_run(server: TmuxServer, command: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: pre(agent, "Bash", "a", command=command)) == ["fight-like-a-man"]
    assert near_now(shown(agent)["@agent_tests_sound_at"])


@rule("S6", "S8")
@pytest.mark.parametrize(
    "command", ["ls", "mypytest", "pytest-cov --help", "echo cargo testing", "npm install"]
)
def test_S6_not_a_test_run(server: TmuxServer, command: str) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: pre(agent, "Bash", "a", command=command)) == []
    assert "@agent_tests_sound_at" not in shown(agent)


@rule("S6")
def test_S6_only_for_bash(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: pre(agent, "Task", "a", description="pytest")) == []


@rule("S6", "S8")
def test_S6_detail_is_cut_before_matching(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    command = "echo " + "x" * 300 + " && pytest"
    assert sounds_after(server, lambda: pre(agent, "Bash", "a", command=command)) == []


@rule("S6")
def test_S6_at_most_every_600_seconds(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    pre(agent, "Bash", "a", command="pytest")
    first = shown(agent)["@agent_tests_sound_at"]
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: pre(agent, "Bash", "b", command="pytest")) == []
    assert shown(agent)["@agent_tests_sound_at"] == first


@rule("S8")
def test_S8_custom_regex(server: TmuxServer) -> None:
    server.set_global("@agent_test_regex", "^just check")
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: pre(agent, "Bash", "a", command="pytest")) == []
    assert sounds_after(server, lambda: pre(agent, "Bash", "b", command="just check")) == [
        "fight-like-a-man"
    ]


# --- S9 playing --------------------------------------------------------------------------


@rule("S9")
def test_S9_sound_switch_off(server: TmuxServer) -> None:
    server.set_global("@agent_sound", "off")
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == []
    server.sink.wait_for(lambda line: line["effect"] == "notify")  # only the sound is off


@rule("S9")
def test_S9_missing_or_unreadable_file_is_silent(server: TmuxServer) -> None:
    sounds = Path(server.env["AG_SOUNDS"])
    (sounds / "need-backup.wav").unlink()
    (sounds / "report-in.wav").chmod(0)
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    assert sounds_after(server, lambda: ask(a, "Bash", "a")) == []
    assert sounds_after(server, lambda: ask(b, "AskUserQuestion", "q")) == []


@rule("S9")
@pytest.mark.parametrize("ext", ["ogg", "oga", "mp3", "flac"])
def test_S9_other_file_types(server: TmuxServer, ext: str) -> None:
    sounds = Path(server.env["AG_SOUNDS"])
    (sounds / "need-backup.wav").rename(sounds / f"need-backup.{ext}")
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == ["need-backup"]


@rule("S9")
def test_S9_lower_or_equal_priority_waits_out_the_window(server: TmuxServer) -> None:
    a, b, c = server.agent("claude"), server.agent("claude"), server.agent("claude")
    for agent in (a, b, c):
        working(agent)
    ask(a, "Bash", "x")
    server.sink.wait_sound("need-backup")  # priority 4
    assert sounds_after(server, lambda: b.hook("StopFailure"), 0.5) == []  # oh-man, 4
    time.sleep(DEBOUNCE)
    assert sounds_after(server, lambda: c.hook("StopFailure")) == ["oh-man"]


@rule("S9")
def test_S9_higher_priority_plays_within_the_window(server: TmuxServer) -> None:
    a, b = server.agent("claude"), server.agent("claude")
    working(a)
    working(b)
    pre(a, "Bash", "t", command="pytest")
    server.sink.wait_sound("fight-like-a-man")  # priority 1
    assert sounds_after(server, lambda: a.hook("Stop"), 0.5) == ["enemy-down"]  # 2
    assert sounds_after(server, lambda: ask(b, "Bash", "x")) == ["need-backup"]  # 4


# --- mute ---------------------------------------------------------------------------


@rule("S1", "S3", "S4", "S6")
def test_S_muted_space_is_silent(server: TmuxServer) -> None:
    server.set_global("@agent_mute_test", "on")
    agent = server.agent("claude")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == []
    assert sounds_after(server, lambda: pre(agent, "Bash", "t", command="pytest")) == []
    assert near_now(shown(agent)["@agent_tests_sound_at"])  # set even when muted
    assert sounds_after(server, lambda: agent.hook("Stop")) == []
    agent.hook("UserPromptSubmit")
    assert sounds_after(server, lambda: agent.hook("StopFailure")) == []


@rule("S1")
def test_S_mute_uses_the_sanitized_space_name(server: TmuxServer) -> None:
    server.session("odd", space="my space.x")
    server.set_global("@agent_mute_my_space_x", "on")
    agent = server.agent("claude", "odd")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == []


@rule("S1")
def test_S_space_falls_back_to_the_detected_one(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(conf=False)  # agent-spaces would detect its own @space_auto
    server.session("auto", space=None)
    server.tmux("set", "-t", "auto", "@space_auto", "detected")
    server.set_global("@agent_mute_detected", "on")
    agent = server.agent("claude", "auto")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == []


@rule("S1")
def test_S_explicit_space_wins_over_the_detected_one(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server(conf=False)
    server.session("both", space="chosen")
    server.tmux("set", "-t", "both", "@space_auto", "detected")
    server.set_global("@agent_mute_detected", "on")
    agent = server.agent("claude", "both")
    working(agent)
    assert sounds_after(server, lambda: ask(agent, "Bash", "a")) == ["need-backup"]
