"""After cutover (tasks F1-F3): what the user does with a key or a focus
change must not make the tmux server fork (a big server's fork() costs
hundreds of ms), and the bar still fits a narrower client.

A thread watches the tmux server's children (harness.procs.ChildWatch); each
scene first shows that it catches a `run-shell true`.
"""

import os
import time
from collections.abc import Callable
from dataclasses import dataclass

import pytest

from harness import procs
from harness.claude import state, working
from harness.impl import IMPL
from harness.marks import rule
from harness.tmux import Terminal, TmuxServer
from harness.wait import eventually

SETTLE = 1.5  # hooks tmux runs after a switch
SPAWN = os.environ.get("AGENTD_TRANSPORT") == "spawn"

no_forks = pytest.mark.skipif(
    IMPL.name != "rust" or SPAWN,
    reason="bash, and agentd's spawn transport, handle these through run-shell by design",
)


@dataclass
class Scene:
    server: TmuxServer
    user: Terminal

    def forks(self, action: Callable[[], object]) -> dict[int, str]:
        """New children of the tmux server during `action` and the hooks it fires."""
        with procs.ChildWatch(self.server.pid) as watch:
            action()
            time.sleep(SETTLE)
        return watch.new


@pytest.fixture
def scene(make_server: Callable[..., TmuxServer]) -> Scene:
    """The user's client on `main`; `work` is session 2 of the same space."""
    server = make_server(focus="terminal", theme=True)
    server.session("work")
    server.run_tool([str(server.impl.bin / "agent-spaces"), "load"])
    assert isinstance(server.client, Terminal)
    time.sleep(SETTLE)
    # A session nobody has shown yet has no layout width, so its first visit
    # lays its bar out (F2: the width differs): once per session for the
    # server's whole life, and there is no width to lay it out for before a
    # client shows it. Visit each once, as in use, before measuring.
    server.client.switch("work")
    time.sleep(SETTLE)
    server.client.switch("main")
    time.sleep(SETTLE)
    s = Scene(server, server.client)
    assert s.forks(lambda: server.tmux("run-shell", "true")), "the watch must see a fork"
    return s


def session_of(user: Terminal) -> str:
    return user.value("#{client_session}")


@rule("E1")
@no_forks
def test_F1_seen_without_a_fork(scene: Scene) -> None:
    server = scene.server
    agent = server.agent("claude", "work")
    server.tmux("select-window", "-t", agent.pane)
    working(agent)
    agent.hook("Stop")
    assert state(agent) == "done"
    control = [name for name, _ in server.control_clients()]
    assert server.global_option("@agentd_client") in control, "the daemon names its client"
    forks = scene.forks(lambda: scene.user.switch("work"))
    eventually(lambda: state(agent), lambda s: s == "idle", 3, "seen")
    assert forks == {}


@no_forks
def test_F_switch_client_without_a_fork(scene: Scene) -> None:
    forks = scene.forks(lambda: scene.user.switch("work"))
    assert session_of(scene.user) == "work"
    assert forks == {}
    forks = scene.forks(lambda: scene.user.switch("main"))
    assert session_of(scene.user) == "main"
    assert forks == {}


@no_forks
def test_F_prefix_g_without_a_fork(scene: Scene) -> None:
    def go(n: str) -> None:
        scene.server.tmux("send-keys", "-K", "-c", scene.user.name, "C-b", "g", n)

    forks = scene.forks(lambda: go("2"))
    assert session_of(scene.user) == "work"
    assert forks == {}
    forks = scene.forks(lambda: go("1"))
    assert session_of(scene.user) == "main"
    assert forks == {}


# --- the bar and a narrower client --------------------------------------------------

LAYOUT = "#{@narrow-tabs}|#{@summary-room}|#{status}|" + "|".join(f"#{{@row{i}}}" for i in range(5))


def layout(server: TmuxServer, session: str) -> str:
    return server.tmux("display", "-p", "-t", session, LAYOUT)


def settled(server: TmuxServer, session: str) -> str:
    """The layout once the hooks are done (unchanged for a second: several
    layouts may run one after another), after checking that a full
    `agent-spaces layout` would not change it: what the hooks left is what
    the clients need."""
    seen = [layout(server, session)]
    still = time.monotonic()
    end = still + 10
    while time.monotonic() - still < 1 and time.monotonic() < end:
        time.sleep(0.1)
        now = layout(server, session)
        if now != seen[-1]:
            seen.append(now)
            still = time.monotonic()
    before = seen[-1]
    server.run_tool([str(server.impl.bin / "agent-spaces"), "layout"])
    time.sleep(0.5)
    assert layout(server, session) == before, "the hooks left a stale bar"
    return before


def room(value: str) -> int:
    """@summary-room: the columns the bar has left, which follow the width."""
    return int(value.split("|")[1] or 0)


def test_F2_a_narrower_client_attaching(scene: Scene) -> None:
    server = scene.server
    wide = settled(server, "main")  # the user's 200 columns
    server.terminal("main", cols=80)
    eventually(lambda: layout(server, "main"), lambda v: room(v) < room(wide), 5, "main laid out for 80 columns")
    settled(server, "main")


def test_F2_a_narrower_client_switching_in(scene: Scene) -> None:
    server = scene.server
    wide = settled(server, "main")
    small = server.terminal("work", cols=80)
    eventually(lambda: layout(server, "work"), lambda v: room(v) < room(wide), 5, "work laid out for 80 columns")
    assert settled(server, "main") == wide  # only the wide client shows main
    small.switch("main")
    eventually(lambda: layout(server, "main"), lambda v: room(v) < room(wide), 5, "main laid out for 80 columns")
    settled(server, "main")


def test_F2_windows_opened_and_closed_under_a_narrow_client(scene: Scene) -> None:
    """A session switch with the same width no longer lays the bar out, so
    opening and closing windows must: long tabs that no longer fit 80
    columns change the bar, and closing them brings it back."""
    server = scene.server
    small = server.terminal("work", cols=80)
    wide = settled(server, "main")
    eventually(lambda: layout(server, "work"), lambda v: room(v) < room(wide), 5, "work laid out for 80 columns")
    before = settled(server, "work")
    windows = [server.tmux("new-window", "-d", "-t", "work:", "-P", "-F", "#{window_id}", "-n",
                           f"a-rather-long-window-name-{i}") for i in range(4)]
    eventually(lambda: layout(server, "work"), lambda v: v != before, 5, "the bar redone for the new tabs")
    settled(server, "work")
    for window in windows:
        server.tmux("kill-window", "-t", window)
    eventually(lambda: layout(server, "work"), lambda v: v == before, 5, "the bar back as it was")
    assert small.value("#{client_session}") == "work"


# --- a tmux 3.6 pitfall ------------------------------------------------------------------


def alive(server: TmuxServer) -> bool:
    return procs.alive(server.pid) and server.tmux("list-sessions", check=False) != ""


def test_F_server_survives_a_client_still_connecting(scene: Scene) -> None:
    """A client that has connected but not yet identified itself has no
    name: in a #{L:} loop over clients tmux 3.6 strdup()s that NULL and the
    server dies. There is nearly always such a client while hooks run (any
    `tmux` command connecting), so the hooks' loops must skip it. Here one
    is held open through a switch, a seen pane and a switch back."""
    import socket

    server = scene.server
    agent = server.agent("claude", "work")
    server.tmux("select-window", "-t", agent.pane)
    working(agent)
    agent.hook("Stop")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as raw:
        raw.connect(server.socket_path)  # and says nothing
        scene.user.switch("work")
        time.sleep(SETTLE)
        assert alive(server), "the tmux server died on the switch to work"
        scene.user.switch("main")
        time.sleep(SETTLE)
        assert alive(server), "the tmux server died on the switch back"


def test_F_server_survives_fast_switch_scenes(scene: Scene) -> None:
    """The scene that found the crash, ten times without pauses and with a
    connection that never identifies itself: a new session, a switch there,
    a finished turn, a switch back to the pane in front."""
    import socket

    server = scene.server
    agent = server.agent("claude", "main")
    server.tmux("select-window", "-t", agent.pane)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as raw:
        raw.connect(server.socket_path)
        for i in range(10):
            server.session(f"quick{i}")
            scene.user.switch(f"quick{i}")
            working(agent)
            agent.hook("Stop")
            scene.user.switch("main")
            assert alive(server), f"the tmux server died in scene {i}"
    time.sleep(SETTLE)
    assert alive(server)
