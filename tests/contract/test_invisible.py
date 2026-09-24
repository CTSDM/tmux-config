"""§16 The daemon is invisible (Z1), for an implementation with a tmux client
of its own (agentd over control mode; bash has none).

A real user client, rendered by an outer tmux, sits on session `main` of
space `test`; `work` is another session of that space, `other` one of
another space. Two fake agents are `done` and already old (no blink): the
one in front in `main`, and the active one of `work`. The user's view (window sizes,
bar layout options, `session_attached`, pane states, where the user's client
is) must not change when the daemon attaches, while it runs, when its
session is killed and when it goes away; the daemon's session must not show
in any list; when only its session is left, the server exits."""

import signal
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import pytest

from harness import procs
from harness.claude import state, working
from harness.impl import IMPL
from harness.marks import rule
from harness.tmux import US, Terminal, TmuxServer
from harness.wait import eventually

pytestmark = pytest.mark.skipif(IMPL.name != "rust", reason="bash talks to tmux without a client of its own")

USER_SESSIONS = ("main", "work", "other")
LAYOUT = "#{session_attached}|#{@narrow-tabs}|#{@summary-room}|#{status}|" + "|".join(
    f"#{{@row{i}}}" for i in range(5)
)
SETTLE = 2.0  # hooks that tmux runs in the background (agent-spaces layout)


@dataclass
class Scene:
    server: TmuxServer
    user: Terminal

    def view(self) -> dict[str, str]:
        """What the user can see, apart from the daemon itself."""
        t = self.server.tmux
        seen: dict[str, str] = {"client": self.user.value("#{client_session} #{client_width}x#{client_height}")}
        for sess in USER_SESSIONS:
            seen[f"{sess} layout"] = t("display", "-p", "-t", sess, LAYOUT)
            seen[f"{sess} windows"] = t("list-windows", "-t", sess, "-F", "#{window_id} #{window_width}x#{window_height}")
            seen[f"{sess} panes"] = t("list-panes", "-s", "-t", sess, "-F", "#{pane_id} #{@agent_state}")
        return seen

    def daemon_sessions(self) -> set[str]:
        return {sess for _, sess in self.server.control_clients()}

    def start(self) -> set[str]:
        """Start the daemon; its sessions."""
        self.server.start_agentd()
        eventually(self.server.control_clients, lambda c: len(c) > 0, 5, "the daemon's control client")
        time.sleep(SETTLE)
        return self.daemon_sessions()


def done_agent(server: TmuxServer, session: str) -> None:
    """A fake agent in front in `session`, finished, not looked at, too old
    to blink. Seeded (P2): a hook would start the daemon."""
    agent = server.agent("claude", session)
    server.tmux("select-window", "-t", agent.pane)
    old = str(int(time.time()) - 1000)
    for name, value in (("@agent", "claude"), ("@agent_state", "done"), ("@agent_since", old)):
        server.tmux("set", "-p", "-t", agent.pane, name, value)


@pytest.fixture
def scene(make_server: Callable[..., TmuxServer]) -> Scene:
    server = make_server(focus="terminal", theme=True, agentd=False)
    server.session("work")
    server.session("other", space="other")
    server.run_tool([str(server.impl.bin / "agent-spaces"), "load"])
    for sess in ("main", "work"):
        done_agent(server, sess)
    # Lay the bars out for these windows, so the baseline is not stale: a
    # later layout (any client-attached hook) would otherwise change it.
    server.run_tool([str(server.impl.bin / "agent-spaces"), "layout"])
    assert isinstance(server.client, Terminal)
    eventually(lambda: server.client.screen()[0] if isinstance(server.client, Terminal) else "",
               lambda row: "work" in row, 5, "the top row")
    time.sleep(SETTLE)
    return Scene(server, server.client)


def daemon_pid(server: TmuxServer) -> int:
    for pid in procs.marked(server.marker):
        if procs.comm(pid) == "agentd" and b"daemon" in Path(f"/proc/{pid}/cmdline").read_bytes():
            return pid
    raise AssertionError("no agentd daemon")


# --- the moments --------------------------------------------------------------------


@rule("Z1")
def test_Z1_attaching_changes_nothing(scene: Scene) -> None:
    before = scene.view()
    assert scene.start(), "the daemon should have a client of its own"
    assert scene.view() == before


@rule("Z1")
def test_Z1_running_changes_nothing(scene: Scene) -> None:
    scene.server.session("busy")
    agent = scene.server.agent("claude", "busy")
    scene.start()
    before = scene.view()
    working(agent)
    agent.hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})  # blinks
    agent.hook("PostToolUse", tool_name="Bash", tool_use_id="a")
    agent.hook("Stop")
    scene.server.reconcile()
    time.sleep(SETTLE)
    assert scene.view() == before


@rule("Z1")
def test_Z1_its_session_killed(scene: Scene) -> None:
    own = scene.start()
    before = scene.view()
    for sess in own:
        scene.server.tmux("kill-session", "-t", sess)
    time.sleep(SETTLE)
    assert scene.view() == before
    scene.server.session("after", space="elsewhere")  # not in the user's bar
    agent = scene.server.agent("claude", "after")
    agent.hook("SessionStart", source="startup")  # still served
    assert state(agent) == "ready"
    time.sleep(SETTLE)
    assert {sess for _, sess in scene.server.control_clients()}.isdisjoint(USER_SESSIONS + ("after",))
    view = scene.view()
    assert view == before


@rule("Z1")
def test_Z1_daemon_goes_away(scene: Scene) -> None:
    own = scene.start()
    before = scene.view()
    pid = daemon_pid(scene.server)
    procs.signal_marked(pid, scene.server.marker, signal.SIGTERM)
    eventually(lambda: procs.alive(pid), lambda alive: not alive, 5, "agentd exited")
    time.sleep(SETTLE)
    sessions = set(scene.server.tmux("list-sessions", "-F", "#{session_name}").split())
    assert not own & sessions, "its session left behind"
    assert scene.server.control_clients() == []
    assert scene.view() == before


@rule("Z1")
def test_Z1_last_user_session_closes(scene: Scene) -> None:
    scene.start()
    pid = daemon_pid(scene.server)
    for sess in USER_SESSIONS:
        scene.server.tmux("kill-session", "-t", sess)

    def server_up() -> bool:
        return scene.server.tmux("list-sessions", check=False) != "" or procs.alive(scene.server.pid)

    eventually(server_up, lambda up: not up, 5, "the server exits as without the daemon")
    eventually(lambda: procs.alive(pid), lambda alive: not alive, 5, "agentd exits")


@rule("Z1")
def test_Z1_plain_attach_lands_in_a_user_session(scene: Scene) -> None:
    own = scene.start()  # its session is the newest now
    other = scene.server.terminal(None)
    assert other.value("#{client_session}") in USER_SESSIONS
    assert other.value("#{client_session}") not in own


# --- lists and choices ------------------------------------------------------------------


@rule("Z1")
def test_Z1_not_in_the_top_row_nor_the_lists(scene: Scene) -> None:
    own = scene.start()
    bin_dir = scene.server.impl.bin
    top = scene.user.screen()[0]
    sessions = scene.server.run_tool([str(bin_dir / "agent-sessions"), "--list", "all"]).stdout
    board = scene.server.run_tool([str(bin_dir / "agent-board"), "--list"]).stdout
    assert "work" in top and "work" in sessions and "work:" in board  # the lists do list
    for name in own:
        assert name not in top
        assert name not in sessions
        assert name not in board


@rule("Z1")
@pytest.mark.parametrize("key", ["s", "w"])
def test_Z1_not_in_tmux_choose_tree(scene: Scene, key: str) -> None:
    own = scene.start()
    scene.user.keys("C-b", key)
    screen = eventually(lambda: "\n".join(scene.user.screen()), lambda text: "work" in text and "(0)" in text,
                        3, f"prefix {key} open")
    scene.user.keys("q")
    for name in own:
        assert name not in screen


@rule("Z1")
def test_Z1_not_in_tmux_choose_client(scene: Scene) -> None:
    scene.start()
    control = [name for name, _ in scene.server.control_clients()]
    scene.user.keys("C-b", "D")
    screen = eventually(lambda: "\n".join(scene.user.screen()), lambda text: scene.user.name in text, 3,
                        "prefix D open")
    scene.user.keys("q")
    for name in control:
        assert name not in screen


@rule("Z1")
@pytest.mark.parametrize(("key", "shows"), [("a", "work:0.0"), ("e", "work")])
def test_Z1_not_in_the_board_and_search_popups(scene: Scene, key: str, shows: str) -> None:
    """Smoke test of the real popups (prefix a, prefix e)."""
    own = scene.start()
    scene.user.keys("C-b", key)
    screen = eventually(lambda: "\n".join(scene.user.screen()[2:]), lambda text: shows in text, 5,
                        f"prefix {key} popup")
    scene.user.keys("Escape")
    for name in own:
        assert name not in screen


@rule("Z1")
def test_Z1_agent_jump_picks_the_user_client(scene: Scene) -> None:
    agent = scene.server.agent("claude", "work")
    own = scene.start()  # the daemon's client is the newest client
    scene.server.run_tool([str(scene.server.impl.bin / "agent-jump"), agent.pane])
    eventually(lambda: scene.user.value("#{client_session}"), lambda s: s == "work", 3, "the user moved")
    assert {sess for _, sess in scene.server.control_clients()} <= own


@rule("Z1")
def test_Z1_agent_next_moves_the_user_to_an_agent(scene: Scene) -> None:
    agent = scene.server.agent("claude", "work")
    own = scene.start()
    working(agent)
    agent.hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})
    scene.server.run_tool([str(scene.server.impl.bin / "agent-next"), scene.user.name])
    eventually(lambda: scene.user.value("#{client_session}"), lambda s: s == "work", 3, "the user moved")
    assert scene.user.value("#{client_session}") not in own


@rule("Z1", "V1")
@pytest.mark.parametrize("focused", [True, False])
def test_Z1_tmux_focus_flag_is_the_users(make_server: Callable[..., TmuxServer], focused: bool) -> None:
    """Without AG_FOCUS_CLIENT, the focused client is the user's, whatever
    flags the daemon's own client carries."""
    server = make_server(focus="flag")
    client = server.client
    assert client is not None
    agent = server.agent("claude")
    server.tmux("select-window", "-t", agent.pane)
    assert server.control_clients(), "the daemon should have a client of its own"
    (client.focus_in if focused else client.focus_out)()
    eventually(lambda: "focused" in client.value("#{client_flags}").split(","), lambda f: f == focused, 3,
               "focus flag")
    working(agent)
    agent.hook("Stop")
    assert state(agent) == ("idle" if focused else "done")


def test_Z1_scene_sanity(scene: Scene) -> None:
    """The scene itself: no daemon yet, a done agent in front of main and work."""
    assert scene.server.control_clients() == []
    for sess in ("main", "work"):
        assert scene.server.tmux("display", "-p", "-t", sess, "#{@agent_state}") == "done"
    assert US not in "".join(scene.view().values())
