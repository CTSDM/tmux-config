"""The harness itself: isolation, fake agents, hooks as their children,
snapshots, sink, client, cleanup. Implementation-agnostic except the smoke test."""

import json
import os
import socket
import subprocess
from collections.abc import Callable
from pathlib import Path

import pytest

from harness import procs
from harness.agent import FakeAgent
from harness.sink import Sink
from harness.tmux import TmuxServer

# Prints the hook's process chain and environment instead of handling the event.
PROBE = [
    "/bin/sh",
    "-c",
    'cat >/dev/null; p=$PPID; while [ "$p" -gt 1 ]; do printf "%s:%s\\n" "$p" "$(cat /proc/$p/comm)"; '
    'p=$(sed "s/.*) //" /proc/$p/stat | cut -d" " -f2); done >&2; env >"$PROBE_OUT"',
]


def probe(agent: FakeAgent, tmp: Path, wrap: int = 0) -> tuple[list[tuple[int, str]], dict[str, str]]:
    out = tmp / f"probe-{agent.pid}.env"
    result = agent.run_hook("{}", argv=PROBE, env={"PROBE_OUT": str(out)}, wrap=wrap)
    chain = [(int(p), c) for p, c in (line.split(":", 1) for line in result.stderr.splitlines())]
    env = dict(line.split("=", 1) for line in out.read_text().splitlines() if "=" in line)
    return chain, env


def test_fake_agents_are_named_and_are_the_pane_process(server: TmuxServer) -> None:
    for kind in ("claude", "codex"):
        agent = server.agent(kind)
        assert procs.comm(agent.pid) == kind
        assert server.pane_pid(agent.pane) == agent.pid


def test_agent_under_a_shell(server: TmuxServer) -> None:
    agent = server.agent("claude", shell=True)
    pane_pid = server.pane_pid(agent.pane)
    assert procs.comm(pane_pid) == "sh"
    assert procs.ppid(agent.pid) == pane_pid


def test_hook_runs_as_a_child_of_the_agent(server: TmuxServer, tmp_path: Path) -> None:
    agent = server.agent("claude", shell=True)
    chain, env = probe(agent, tmp_path)
    assert chain[0] == (agent.pid, "claude")
    assert chain[1] == (server.pane_pid(agent.pane), "sh")
    assert env["TMUX_PANE"] == agent.pane

    chain, _ = probe(agent, tmp_path, wrap=3)
    assert [c for _, c in chain[:4]] == ["sh", "sh", "sh", "claude"]


def test_nested_agent(server: TmuxServer, tmp_path: Path) -> None:
    outer = server.agent("claude")
    inner = outer.child_agent("claude")
    assert inner.pane == outer.pane
    assert procs.ppid(inner.pid) == outer.pid
    chain, _ = probe(inner, tmp_path)
    assert [c for _, c in chain[:2]] == ["claude", "claude"]


def test_environment_is_isolated(server: TmuxServer, tmp_path: Path) -> None:
    agent = server.agent("claude")
    _, env = probe(agent, tmp_path)
    live = os.environ.get("TMUX", "").split(",")[0]
    assert env["TMUX"].split(",")[0] == server.socket_path != live
    assert server.name in server.socket_path
    for var in ("HOME", "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "AG_SINK", "AG_SOUNDS"):
        assert env[var].startswith(str(server.root)), var
    assert "DBUS_SESSION_BUS_ADDRESS" not in env
    assert server.bus.path == Path(env["XDG_RUNTIME_DIR"]) / "bus"
    assert env["AG_FOCUS_CLIENT"] == "none"
    assert env[procs.MARKER_VAR] == server.marker
    leaked = [k for k in env if k.startswith(("CLAUDE", "CODEX", "HYPRLAND", "WAYLAND"))]
    assert leaked == []
    # The seams are in the server's global environment too (tmux hooks use it).
    global_env = server.tmux("show-environment", "-g").splitlines()
    assert f"AG_SINK={server.sink.path}" in global_env
    assert "AG_FOCUS_CLIENT=none" in global_env


def test_option_snapshot_is_raw(server: TmuxServer) -> None:
    agent = server.agent("claude")
    tricky = {
        "@agent_msg": 'said "hi" ## $HOME \\ it\'s',
        "@agent_tool": "✳ ünï\ttab",
        "@agent_empty": "",
        "@other": "x",
    }
    for name, value in tricky.items():
        server.tmux("set", "-p", "-t", agent.pane, name, value)
    assert server.pane_options(agent.pane) == tricky
    assert "@other" not in agent.options()
    assert agent.option("@agent_msg") == tricky["@agent_msg"]
    assert agent.option("@agent_unset") == ""


def test_kill_leaves_nothing_behind(make_server: Callable[..., TmuxServer]) -> None:
    server = make_server()
    agent = server.agent("codex")
    # A detached grandchild in its own session, like the helpers hooks start.
    agent.spawn(["setsid", "-f", "sleep", "300"], new_session=True)
    assert len(procs.marked(server.marker)) >= 3  # server, pane, agent, sleep...
    socket_file = Path(server.socket_path)
    server.kill()
    assert procs.marked(server.marker) == []
    assert not socket_file.exists()
    assert subprocess.run(["tmux", "-L", server.name, "ls"], env=server.env, capture_output=True).returncode != 0


def test_attached_client_and_focus(make_server: Callable[..., TmuxServer], tmp_path: Path) -> None:
    server = make_server(focus="client")
    client = server.client
    assert client is not None
    agent = server.agent("claude")
    _, env = probe(agent, tmp_path)
    assert env["AG_FOCUS_CLIENT"] == client.name
    assert client.value("#{client_session}") == "main"
    client.focus_out()
    server_flags = lambda: client.value("#{client_flags}").split(",")  # noqa: E731
    from harness.wait import eventually

    eventually(server_flags, lambda f: "focused" not in f, 3, "focus out")
    client.focus_in()
    eventually(server_flags, lambda f: "focused" in f, 3, "focus in")


def test_focus_flag_mode_unsets_the_override(make_server: Callable[..., TmuxServer], tmp_path: Path) -> None:
    server = make_server(focus="flag")
    _, env = probe(server.agent("claude"), tmp_path)
    assert "AG_FOCUS_CLIENT" not in env


def test_sink_reader(tmp_path: Path) -> None:
    sink = Sink(tmp_path / "sink.jsonl")
    assert sink.lines() == []
    with sink.path.open("a") as f:
        f.write(json.dumps({"t": 1, "effect": "sound", "name": "need-backup"}) + "\n")
        f.write(json.dumps({"t": 2, "effect": "notify", "pane": "%3", "urgency": "critical"}) + "\n")
        f.write('{"t": 3, "effect": "sou')  # still being written
    assert sink.sounds() == ["need-backup"]
    assert [n["pane"] for n in sink.notifications("%3")] == ["%3"]
    mark = sink.mark()
    assert sink.since(mark) == []
    with pytest.raises(AssertionError):
        sink.wait_sound("ct-win", timeout=0.2)


def test_tripwires_notice_real_attempts(server: TmuxServer) -> None:
    subprocess.run([server.env["AG_SOUND_PLAYER"], "file.wav"], check=True)
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.connect(str(server.bus.path))
    from harness.wait import eventually

    eventually(server.tripped, lambda p: len(p) == 2, 2, "both tripwires")
    # Reset them, or the fixture fails this test.
    server.player_log.unlink()
    server.bus.hits = 0


def test_hook_smoke(server: TmuxServer) -> None:
    """The implementation under test publishes a SessionStart (contract H1, H14)."""
    agent = server.agent("claude")
    result = agent.hook("SessionStart", source="startup", model="test-model")
    assert result.rc == 0
    options = agent.options()
    assert options["@agent"] == "claude"
    assert options["@agent_state"] == "ready"
    assert options["@agent_session"] == agent.session_id
    assert options["@agent_model"] == "test-model"


def test_the_tmux_under_test(server: TmuxServer, tmp_path: Path) -> None:
    """The server, the harness and the hooks all run the same tmux (AGENT_TMUX when set)."""
    import shutil as sh

    from harness.tmux import TMUX_BIN

    tmux = str(TMUX_BIN or sh.which("tmux"))
    expected = subprocess.run([tmux, "-V"], capture_output=True, text=True).stdout.split()[-1]
    assert server.tmux("display", "-p", "#{version}") == expected
    _, env = probe(server.agent("claude"), tmp_path)
    found = subprocess.run(["sh", "-c", "command -v tmux"], env=env, capture_output=True, text=True).stdout.strip()
    assert Path(found).resolve() == Path(tmux).resolve()
