"""T5.5: dress rehearsal of the cutover to agentd and of the rollback, on an
isolated server with a real install of the daemon branch in a temporary
HOME (harness/rehearsal.py). Not in the default run (it builds agentd and
clones TPM and tmuxifier):

    uv run pytest rehearsal            # COMMIT=<sha> to rehearse another one

Agents "started before" a switch keep the hook command they read at start:
the old ones call the bash agent-hook, the new ones `agentd hook`.
"""

import json
import os
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

from harness import procs
from harness.agent import FakeAgent
from harness.claude import state
from harness.codex import Codex, Rollout
from harness.rehearsal import Install, clone, impl_of
from harness.tmux import US, Client, TmuxServer
from harness.wait import eventually

COMMIT = os.environ.get("COMMIT", "57583e8")

type Hooks = dict[str, list[str]]

# What the user had before: other tools' hooks and settings.
FOREIGN_CLAUDE: dict[str, Any] = {
    "model": "opus",
    "permissions": {"allow": ["Bash(ls:*)"]},
    "hooks": {
        "Stop": [{"matcher": "*", "hooks": [{"type": "command", "command": "notify-me --foreign"}]}],
        "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
    },
}
FOREIGN_CODEX: dict[str, Any] = {
    "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "codex-foreign"}]}]}
}


# --- the install ------------------------------------------------------------------


def commands(settings: Path) -> Hooks:
    hooks: dict[str, Any] = json.loads(settings.read_text()).get("hooks", {})
    return {
        event: [h.get("command", "") for group in groups for h in group.get("hooks", [])]
        for event, groups in hooks.items()
    }


def is_ours(command: str) -> bool:
    return "agents/bin/agent-hook " in command + " " or " hook claude" in command or " hook codex" in command


def check_install(inst: Install, mode: str) -> None:
    """Every hooked event has exactly one entry of ours, of `mode`; the
    user's other hooks and settings are still there."""
    for kind, files in (
        ("claude", [inst.home / ".claude" / "settings.json", inst.home / ".config" / "claude" / "work" / "settings.json"]),
        ("codex", [inst.home / ".codex" / "hooks.json"]),
    ):
        suffix = f"agents/bin/agent-hook {kind}" if mode == "bash" else f"agentd hook {kind}"
        for settings in files:
            hooks = commands(settings)
            for event in ("SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest",
                          "Stop", "SessionEnd"):
                mine = [c for c in hooks.get(event, []) if is_ours(c)]
                assert len(mine) == 1 and mine[0].endswith(suffix), f"{settings} {event}: {mine}"
            for event, cmds in hooks.items():
                assert len([c for c in cmds if is_ours(c)]) <= 1, f"{settings} {event}: duplicates {cmds}"
    claude = json.loads((inst.home / ".claude" / "settings.json").read_text())
    assert claude["model"] == "opus" and claude["permissions"] == FOREIGN_CLAUDE["permissions"]
    assert "notify-me --foreign" in commands(inst.home / ".claude" / "settings.json")["Stop"]
    assert "guard.sh" in commands(inst.home / ".claude" / "settings.json")["PreToolUse"]
    assert "codex-foreign" in commands(inst.home / ".codex" / "hooks.json")["Stop"]


# --- what runs --------------------------------------------------------------------


def daemons(server: TmuxServer) -> list[int]:
    return [p for p in procs.marked(server.marker)
            if procs.comm(p) == "agentd" and b"daemon" in Path(f"/proc/{p}/cmdline").read_bytes()]


def animators(server: TmuxServer) -> list[int]:
    """Bash animators (the daemon's own is inside it)."""
    return [p for p in procs.marked(server.marker)
            if b"agent-blink" in Path(f"/proc/{p}/cmdline").read_bytes() and procs.comm(p) == "bash"]


def codex_dirs(server: TmuxServer) -> dict[str, float]:
    """Bash's Codex bookkeeping: file -> mtime."""
    run = Path(server.env["XDG_RUNTIME_DIR"]) / "tmux-agents"
    return {str(f.relative_to(run)): f.stat().st_mtime for f in run.glob("codex-*/**/*") if f.is_file()}


def sessions(server: TmuxServer) -> list[str]:
    return server.tmux("list-sessions", "-F", "#{session_name}").split()


def blink_cycle(server: TmuxServer, session: str, seconds: float = 4) -> float:
    """Median time between the starts of the dark phases of a session's blink."""
    fmt = f"#{{@blink-s-lit}}{US}#{{@blink-s}}"
    samples: list[tuple[float, str]] = []
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        lit, _, _ = server.tmux("display", "-p", "-t", session, fmt).partition(US)
        samples.append((time.monotonic(), lit))
    starts = [t for (t, lit), (_, before) in zip(samples[1:], samples) if lit == "" and before != ""]
    gaps = sorted(b - a for a, b in zip(starts, starts[1:]))
    assert gaps, "no blink cycle seen"
    return gaps[len(gaps) // 2]


def notes(server: TmuxServer, pane: str, effect: str, after: int = 0) -> list[dict[str, Any]]:
    return [line for line in server.sink.since(after) if line.get("pane") == pane and line["effect"] == effect]


def look_at(server: TmuxServer, client: Client, agent: FakeAgent, session: str) -> None:
    """The user brings the pane in front (pane-focus-in)."""
    server.tmux("select-window", "-t", agent.pane)
    client.switch(session)
    time.sleep(1)
    client.switch("main")


# --- the rehearsal -------------------------------------------------------------------


def test_cutover_and_rollback(make_server: Callable[..., TmuxServer]) -> None:
    server = make_install_server(make_server)
    inst = Install(server)
    problems: list[str] = []  # checks that should not stop the later stages
    client = server.client
    assert isinstance(client, Client)
    for profile in (inst.home / ".claude", inst.home / ".config" / "claude" / "work"):
        profile.mkdir(parents=True, exist_ok=True)
        (profile / "settings.json").write_text(json.dumps(FOREIGN_CLAUDE))
    (inst.home / ".codex").mkdir(parents=True, exist_ok=True)
    (inst.home / ".codex" / "hooks.json").write_text(json.dumps(FOREIGN_CODEX))

    def bash_hook(kind: str) -> list[str]:
        return [str(inst.config / "agents" / "bin" / "agent-hook"), kind]

    def agentd_hook(kind: str) -> list[str]:
        return [str(inst.agentd), "hook", kind]


    # 0. Before: the bash setup (hooks on agent-hook, no agentd built).
    done = inst.run(inst.config / "agents" / "install", "--bash")
    assert done.returncode == 0, done.stderr
    check_install(inst, "bash")
    inst.reload(tpm=False)
    assert server.global_option("@agentd") == ""
    server.session("old")
    old = server.agent("claude", "old")
    old.hook("SessionStart", source="startup", _argv=bash_hook("claude"))
    old.hook("UserPromptSubmit", _argv=bash_hook("claude"))
    old.hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"},
             _argv=bash_hook("claude"))
    old_codex = Codex(server.agent("codex", "old"), Rollout(server.root / "old-rollout.jsonl"))
    old_codex.hook("SessionStart", source="startup", _argv=bash_hook("codex"))
    old_codex.hook("UserPromptSubmit", _argv=bash_hook("codex"))
    old_codex.rollout.started("one")
    old_codex.hook("PreToolUse", tool_name="Bash", tool_use_id="c1", tool_input={"command": "make"},
                   _argv=bash_hook("codex"))
    eventually(lambda: notes(server, old.pane, "notify"), lambda n: len(n) == 1, 5, "bash notification")
    assert old.option("@agent_notify_id"), "bash keeps its notification's id"
    assert codex_dirs(server), "bash Codex bookkeeping"
    eventually(lambda: animators(server), lambda a: len(a) == 1, 5, "the bash animator")

    # 1. The switch: setup.sh (agentd built, hooks on agentd), prefix R.
    inst.setup_sh()
    assert inst.agentd.exists()
    check_install(inst, "agentd")
    inst.reload()
    assert server.global_option("@agentd") == str(inst.agentd)
    eventually(lambda: daemons(server), lambda d: len(d) == 1, 5, "agentd started by the config")
    assert "_peek-agentd" in sessions(server)
    bookkeeping = codex_dirs(server)

    # Old agents keep working through agent-hook -> agentd hook.
    old_codex.hook("PostToolUse", tool_name="Bash", tool_use_id="c1", _argv=bash_hook("codex"))
    old_codex.hook("Stop", last_assistant_message="old codex done", _argv=bash_hook("codex"))
    assert state(old_codex.agent) == "done"
    # One animator: the blink of the old needs pane is not doubled.
    cycle = blink_cycle(server, "old")
    assert 0.6 <= cycle <= 1.26, f"blink cycle {cycle:.2f} s: two animators?"
    # A notification bash showed before the switch closes when seen.
    mark = server.sink.mark()
    look_at(server, client, old, "old")
    assert notes(server, old.pane, "notify-close", mark), "pre-cutover notification not closed when seen"
    old.hook("PostToolUse", tool_name="Bash", tool_use_id="a", _argv=bash_hook("claude"))
    assert state(old) == "working"
    old.hook("Stop", last_assistant_message="old claude done", _argv=bash_hook("claude"))
    assert state(old) in ("done", "idle")

    # New agents use agentd hook.
    server.session("new")
    new = server.agent("claude", "new")
    new.hook("SessionStart", source="startup", _argv=agentd_hook("claude"))
    new.hook("UserPromptSubmit", _argv=agentd_hook("claude"))
    new.hook("Stop", last_assistant_message="new claude done", _argv=agentd_hook("claude"))
    assert state(new) == "done"
    late = server.agent("claude", "new")  # its only notification is the daemon's
    late.hook("SessionStart", source="startup", _argv=agentd_hook("claude"))
    late.hook("UserPromptSubmit", _argv=agentd_hook("claude"))
    late.hook("Stop", last_assistant_message="late done", _argv=agentd_hook("claude"))
    eventually(lambda: notes(server, late.pane, "notify"), lambda n: len(n) == 1, 5, "agentd notification")
    new_codex = Codex(server.agent("codex", "new"), Rollout(server.root / "new-rollout.jsonl"))
    new_codex.hook("SessionStart", source="startup", _argv=agentd_hook("codex"))
    new_codex.hook("UserPromptSubmit", _argv=agentd_hook("codex"))
    new_codex.rollout.started("one")
    assert state(new_codex.agent) == "working"
    time.sleep(3)  # an observation tick
    # One owner: bash's Codex bookkeeping is not touched after the switch.
    assert codex_dirs(server) == bookkeeping, "bash Codex bookkeeping after the switch"
    assert len(daemons(server)) == 1

    # 2. Rollback, in the guide's order.
    inst.off.parent.mkdir(parents=True, exist_ok=True)
    inst.off.touch()
    done = inst.run(inst.config / "agents" / "install", "--bash")
    assert done.returncode == 0, done.stderr
    check_install(inst, "bash")
    server.unset_global("@agentd")
    stop = inst.run(inst.agentd, "ctl", "stop", tmux=True)
    assert stop.returncode == 0, stop.stderr
    eventually(lambda: daemons(server), lambda d: d == [], 5, "agentd stopped")
    eventually(lambda: sessions(server), lambda s: "_peek-agentd" not in s, 5, "its session gone")
    run = Path(server.env["XDG_RUNTIME_DIR"]) / "tmux-agents"
    assert not list(run.glob("agentd-*.sock")), "agentd socket left"

    # Agents started with agentd hook go to bash now; no daemon comes back.
    new.hook("UserPromptSubmit", _argv=agentd_hook("claude"))
    new.hook("PermissionRequest", tool_name="Bash", tool_use_id="b", tool_input={"command": "rm x"},
             _argv=agentd_hook("claude"))
    assert state(new) == "needs"
    eventually(lambda: notes(server, new.pane, "notify"), lambda n: len(n) >= 2, 5, "bash notification")
    assert new.option("@agent_notify_id"), "bash shows it"
    before_codex = set(codex_dirs(server))
    new_codex.hook("PreToolUse", tool_name="Bash", tool_use_id="n1", tool_input={"command": "ls"},
                   _argv=agentd_hook("codex"))
    assert state(new_codex.agent) == "working"
    assert set(codex_dirs(server)) - before_codex, "bash Codex bookkeeping for the agentd-hooked pane"
    old.hook("UserPromptSubmit", _argv=bash_hook("claude"))
    assert state(old) == "working"
    eventually(lambda: animators(server), lambda a: len(a) == 1, 5, "the bash animator again")
    cycle = blink_cycle(server, "new")
    assert 0.6 <= cycle <= 1.26, f"blink cycle {cycle:.2f} s: two animators?"
    assert daemons(server) == []
    mark = server.sink.mark()
    look_at(server, client, new, "new")
    assert notes(server, new.pane, "notify-close", mark), "notification not closed when seen"
    # The daemon's notification from before the rollback: closed at `ctl stop`, or when seen.
    look_at(server, client, late, "new")
    if not notes(server, late.pane, "notify-close"):
        problems.append("a notification agentd showed before the rollback is closed neither by "
                        "`ctl stop` nor when seen afterwards")

    # 3. setup.sh and prefix R while agentd.off is there: bash.
    output = inst.setup_sh()
    assert "switched off" in output
    check_install(inst, "bash")
    inst.reload()
    assert server.global_option("@agentd") == ""
    assert daemons(server) == []

    # 4. Back to agentd: delete agentd.off, setup.sh, prefix R.
    inst.off.unlink()
    inst.setup_sh()
    check_install(inst, "agentd")
    inst.reload()
    assert server.global_option("@agentd") == str(inst.agentd)
    eventually(lambda: daemons(server), lambda d: len(d) == 1, 5, "agentd back")

    # 5. The server goes: nothing is left.
    pid = daemons(server)[0]
    server.tmux("kill-server", check=False)
    eventually(lambda: procs.alive(pid), lambda alive: not alive, 5, "agentd exits with its server")
    assert not list(run.glob("agentd-*.sock")), "agentd socket left"
    assert problems == []


def make_install_server(make_server: Callable[..., TmuxServer]) -> TmuxServer:
    """A test server whose HOME holds a clone of COMMIT at ~/.config/tmux.
    The clone must exist before the server starts: the server's global
    environment (HOME) is fixed then."""
    server = make_server(focus="client", conf=False, agentd=False, xdg_in_home=True)
    config = clone(COMMIT, Path(server.env["HOME"]) / ".config" / "tmux")
    server.tmux("set", "-g", "@agents_bin", str(impl_of(config).bin))
    return server
