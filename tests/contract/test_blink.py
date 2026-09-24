"""§10 Turn signal (K1-K5), seen through the @blink-* session and window options."""

import math
import time
from collections.abc import Callable

import pytest

from harness import procs
from harness.agent import FakeAgent
from harness.claude import ask, shown, working
from harness.marks import rule
from harness.tmux import US, TmuxServer
from harness.wait import eventually

REFRESH = 2.0  # "within one refresh" (6 frames of 140 ms), with margin
SESSION = ("@blink-s", "@blink-s-kind", "@blink-s-lit", "@blink-s-rest")
WINDOW = ("@blink-w", "@blink-w-kind", "@blink-w-lit", "@blink-w-rest")


def window_of(server: TmuxServer, agent: FakeAgent) -> str:
    return server.tmux("display", "-p", "-t", agent.pane, "#{window_id}")


def session_blink(server: TmuxServer, session: str) -> dict[str, str]:
    return {k: v for k, v in server.session_options(session).items() if k in SESSION}


def window_blink(server: TmuxServer, window: str) -> dict[str, str]:
    return {k: v for k, v in server.window_options(window).items() if k in WINDOW}


def kind(options: dict[str, str], scope: str) -> str | None:
    """The blink kind, or None when the target is not blinking."""
    return options[f"@blink-{scope}-kind"] if options.get(f"@blink-{scope}") == "1" else None


def wait_session(server: TmuxServer, session: str, expected: str | None, timeout: float = REFRESH) -> None:
    eventually(lambda: kind(session_blink(server, session), "s"), lambda k: k == expected, timeout,
               f"session {session} blinks {expected}")


def wait_window(server: TmuxServer, window: str, expected: str | None, timeout: float = REFRESH) -> None:
    eventually(lambda: kind(window_blink(server, window), "w"), lambda k: k == expected, timeout,
               f"window {window} blinks {expected}")


def done(agent: FakeAgent) -> None:
    working(agent)
    agent.hook("Stop")
    assert shown(agent)["@agent_state"] == "done"


# --- K1, K2 targets and options -----------------------------------------------------


@rule("K1", "K2", "K5")
def test_K1_needs_blinks_its_session_and_window(server: TmuxServer) -> None:
    agent = server.agent("claude")
    window = window_of(server, agent)
    working(agent)
    ask(agent, "Bash", "a")
    wait_session(server, "main", "needs")
    wait_window(server, window, "needs")
    idle_window = server.tmux("display", "-p", "-t", "main:^", "#{window_id}")
    assert window_blink(server, idle_window) == {}
    agent.hook("PostToolUse", tool_name="Bash", tool_use_id="a")
    wait_session(server, "main", None)
    wait_window(server, window, None)
    assert session_blink(server, "main") == {} and window_blink(server, window) == {}


@rule("K1", "K2", "K5")
def test_K1_unseen_work_blinks(server: TmuxServer) -> None:
    agent = server.agent("claude")
    done(agent)
    wait_session(server, "main", "unseen")
    wait_window(server, window_of(server, agent), "unseen")
    agent.hook("UserPromptSubmit")
    wait_session(server, "main", None)


@rule("K1")
def test_K1_unseen_only_for_a_while(server: TmuxServer) -> None:
    server.set_global("@agent_unseen_blink_for", "2")
    agent = server.agent("claude")
    done(agent)
    wait_session(server, "main", "unseen")
    wait_session(server, "main", None, timeout=2 + REFRESH)
    assert shown(agent)["@agent_state"] == "done"


@rule("K1", "P2")
@pytest.mark.parametrize(("limit", "blinks"), [(None, False), ("0", True)])
def test_K1_unseen_limit_zero_is_no_limit(server: TmuxServer, limit: str | None, blinks: bool) -> None:
    if limit is not None:
        server.set_global("@agent_unseen_blink_for", limit)
    agent = server.agent("claude")
    done(agent)
    wait_session(server, "main", "unseen")
    server.tmux("set", "-p", "-t", agent.pane, "@agent_since", str(int(time.time()) - 1000))
    time.sleep(REFRESH)
    assert kind(session_blink(server, "main"), "s") == ("unseen" if blinks else None)


@rule("K1")
def test_K1_needs_wins(server: TmuxServer) -> None:
    finished, waiting = server.agent("claude"), server.agent("claude")
    done(finished)
    working(waiting)
    ask(waiting, "Bash", "a")
    wait_session(server, "main", "needs")
    wait_window(server, window_of(server, waiting), "needs")
    wait_window(server, window_of(server, finished), "unseen")


@rule("K1", "A4", "B1")
def test_K1_no_unseen_blink_with_background_work(server: TmuxServer) -> None:
    with_sub, with_shell = server.agent("claude"), server.agent("claude")
    working(with_sub)
    with_sub.hook("SubagentStart", agent_id="s", agent_type="Explore")
    with_sub.hook("Stop")
    working(with_shell)
    with_shell.spawn(["/bin/bash", "-c", "source /x/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 300; exit 0"])
    with_shell.hook("Stop")
    eventually(lambda: shown(with_shell).get("@agent_bg"), lambda v: v == "1", 5, "@agent_bg")
    time.sleep(REFRESH)
    assert session_blink(server, "main") == {}


@rule("K1")
def test_K1_peek_sessions_never_blink(server: TmuxServer) -> None:
    """A peek session shares the windows of the session it shows (a session group)."""
    agent = server.agent("claude")
    server.tmux("new-session", "-d", "-t", "main", "-s", "_peek-1")
    working(agent)
    ask(agent, "Bash", "a")
    wait_session(server, "main", "needs")
    wait_window(server, window_of(server, agent), "needs")
    time.sleep(REFRESH)
    assert session_blink(server, "_peek-1") == {}


@rule("K1", "K5")
def test_K1_demo(server: TmuxServer) -> None:
    window = server.tmux("display", "-p", "-t", "main:^", "#{window_id}")
    server.start_tool(server.impl.blink_demo_argv("main", window, 3))
    eventually(lambda: server.global_option("@blink-demo"), lambda v: v.startswith(f"main {window} "),
               2, "@blink-demo set")
    wait_session(server, "main", "needs")
    wait_window(server, window, "needs")
    wait_session(server, "main", None, timeout=3 + REFRESH)
    assert window_blink(server, window) == {}
    assert server.global_option("@blink-demo") == ""


# --- K3, K4 text and frames -------------------------------------------------------------


def sample(server: TmuxServer, target: str, scope: str, seconds: float) -> list[tuple[float, str, str]]:
    """(time, lit, rest) of a blinking target, as often as tmux answers."""
    fmt = f"#{{@blink-{scope}-lit}}{US}#{{@blink-{scope}-rest}}"
    samples: list[tuple[float, str, str]] = []
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        lit, _, rest = server.tmux("display", "-p", "-t", target, fmt).partition(US)
        samples.append((time.monotonic(), lit, rest))
    return samples


def sample_both(
    server: TmuxServer, sessions: tuple[str, str], seconds: float
) -> tuple[list[tuple[float, str, str]], list[tuple[float, str, str]]]:
    """sample() of two sessions in the same loop, so both see the same load."""
    fmt = f"#{{@blink-s-lit}}{US}#{{@blink-s-rest}}"
    first: list[tuple[float, str, str]] = []
    second: list[tuple[float, str, str]] = []
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        for session, into in ((sessions[0], first), (sessions[1], second)):
            lit, _, rest = server.tmux("display", "-p", "-t", session, fmt).partition(US)
            into.append((time.monotonic(), lit, rest))
    return first, second


def allowed_lengths(n: int) -> set[int]:
    return {math.ceil(n * (f + 1) / 6) for f in range(6)} | {n, 0}


def cycle(samples: list[tuple[float, str, str]]) -> float:
    """Median time between the starts of dark phases (frame 9)."""
    starts = [t for (t, lit, _), (_, before, _) in zip(samples[1:], samples) if lit == "" and before != ""]
    gaps = sorted(b - a for a, b in zip(starts, starts[1:]))
    assert gaps, "fewer than two cycles seen"
    return gaps[len(gaps) // 2]


def check_frames(samples: list[tuple[float, str, str]], text: str) -> None:
    assert samples
    lengths = {len(lit) for _, lit, _ in samples}
    for _, lit, rest in samples:
        assert lit + rest == text, (lit, rest, text)
    assert lengths <= allowed_lengths(len(text)), lengths
    assert {0, len(text)} <= lengths


@pytest.mark.xdist_group("blink-frames")
@rule("K3", "K4")
def test_K3_session_text_and_frames(server: TmuxServer) -> None:
    name = "a-rather-long-session-name"
    server.session(name)
    agent = server.agent("claude", name)
    working(agent)
    ask(agent, "Bash", "a")
    wait_session(server, name, "needs")
    text = server.tmux("display", "-p", "-t", name, "#{=/16/…:session_name}")
    assert len(text) == 17
    check_frames(sample(server, name, "s", 2.5), text)


@pytest.mark.xdist_group("blink-frames")
@rule("K3", "K4")
@pytest.mark.parametrize(("narrow", "width"), [(False, 22), (True, 14)])
def test_K3_window_text_and_frames(server: TmuxServer, narrow: bool, width: int) -> None:
    if narrow:
        server.tmux("set", "-t", "main", "@narrow-tabs", "1")
    agent = server.agent("claude")
    server.tmux("select-pane", "-t", agent.pane, "-T", "✳ Refactor the CSV exporter module")
    window = window_of(server, agent)
    working(agent)
    ask(agent, "Bash", "a")
    wait_window(server, window, "needs")
    text = server.tmux("display", "-p", "-t", window, f"#{{=/{width}/…:#{{E:@agent-task}}}}")
    assert text.startswith("Refactor") and len(text) == width + 1
    check_frames(sample(server, window, "w", 2.5), text)


# A 12-frame cycle takes 0.84 s at 70 ms a frame and 1.68 s at 140 ms. A
# loaded machine only makes frames late, so the bounds tell the two speeds
# apart (at their midpoint, 1.26 s) and leave room above.
FAST = (0.6, 1.26)
SLOW = (1.26, 2.6)


def between(value: float, bounds: tuple[float, float]) -> bool:
    return bounds[0] <= value <= bounds[1]


@rule("K4", "K5")
@pytest.mark.xdist_group("blink-frames")
def test_K4_needs_cycle_is_12_frames_of_70_ms(server: TmuxServer) -> None:
    agents = [server.agent("claude") for _ in range(3)]
    for agent in agents:  # three triggers: still one animator
        working(agent)
        ask(agent, "Bash", "a")
    wait_session(server, "main", "needs")
    period = cycle(sample(server, "main", "s", 4))
    assert between(period, FAST), period


@rule("K4")
@pytest.mark.xdist_group("blink-frames")
def test_K4_unseen_only_cycle_is_half_speed(server: TmuxServer) -> None:
    agent = server.agent("claude")
    done(agent)
    wait_session(server, "main", "unseen")
    period = cycle(sample(server, "main", "s", 6))
    assert between(period, SLOW), period


@rule("K4")
@pytest.mark.xdist_group("blink-frames")
def test_K4_unseen_next_to_needs_advances_every_other_frame(server: TmuxServer) -> None:
    server.session("other")
    finished, waiting = server.agent("claude"), server.agent("claude", "other")
    done(finished)
    working(waiting)
    ask(waiting, "Bash", "a")
    wait_session(server, "other", "needs")
    wait_session(server, "main", "unseen")
    fast, slow = sample_both(server, ("other", "main"), 6)
    # The speed of a frame is the test above; here, unseen takes every other one.
    assert 1.6 <= cycle(slow) / cycle(fast) <= 2.5, (cycle(fast), cycle(slow))


# --- K5 life -----------------------------------------------------------------------------


def busy_ns(server: TmuxServer, seconds: float) -> int:
    """CPU the server's processes use over `seconds`, fake agents aside, in
    ns (schedstat: an animator in the daemon uses too little for clock ticks)."""
    def cpu() -> dict[int, int]:
        pids = [p for p in procs.marked(server.marker) if procs.comm(p) not in ("claude", "codex", "node")]
        return procs.cpu_ns(pids)

    before = cpu()
    time.sleep(seconds)
    after = cpu()
    return sum(t - before.get(pid, 0) for pid, t in after.items())


@rule("K5")
def test_K5_nothing_runs_without_targets(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    ask(agent, "Bash", "a")
    wait_session(server, "main", "needs")
    assert busy_ns(server, 1) > 0  # the animator is visible to this measure
    agent.hook("PostToolUse", tool_name="Bash", tool_use_id="a")
    wait_session(server, "main", None)
    time.sleep(1)
    assert busy_ns(server, 3) <= 500_000  # 0.5 ms in 3 s: a stray wakeup at most


@rule("K5", "P2")
def test_K5_starts_when_the_configuration_loads(server: TmuxServer) -> None:
    agent = server.agent("claude")
    working(agent)
    server.tmux("set", "-p", "-t", agent.pane, "@agent_state", "needs")  # no event: nothing blinks
    time.sleep(REFRESH)
    assert session_blink(server, "main") == {}
    server.tmux("source-file", str(server.impl.conf))
    wait_session(server, "main", "needs", timeout=5)


@rule("K1")
def test_K1_blink_follows_seen(make_server: Callable[..., TmuxServer]) -> None:
    """E1 turns done into idle: the unseen blink stops."""
    server = make_server(focus="client")
    assert server.client is not None
    server.client.focus_in()
    agent = server.agent("claude")
    done(agent)
    wait_session(server, "main", "unseen")
    server.tmux("select-window", "-t", agent.pane)
    wait_session(server, "main", None)
