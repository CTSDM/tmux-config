"""I5 A Claude session parked in the background (CHANGE C11): it runs outside
the pane, and Claude Code's session registry says which pane shows it."""

import json
import subprocess
import time
from collections.abc import Iterator
from pathlib import Path

import pytest

from harness import procs
from harness.agent import FakeAgent, agent_argv, wait_for_socket
from harness.claude import shown, state
from harness.marks import change, rule
from harness.tmux import TmuxServer

JOB = "4db52de7"
# A shell of Claude's Bash tool run in the background, as test_background's.
SHELL = "source /nonexistent/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 300; exit 0"


def background_shell(agent: FakeAgent) -> int:
    return agent.spawn(["/bin/bash", "-c", SHELL])


def wait_bg(agent: FakeAgent, value: str, timeout: float = 5.0) -> None:
    end = time.monotonic() + timeout
    while shown(agent).get("@agent_bg", "") != value:
        assert time.monotonic() < end, f"@agent_bg is {shown(agent).get('@agent_bg')!r}, expected {value!r}"
        time.sleep(0.1)


class Parked:
    """The pane's `claude` (the viewer) and the session it shows, a fake
    agent started outside tmux, as Claude Code's daemon runs it."""

    def __init__(self, server: TmuxServer, config: Path) -> None:
        self.config = config
        self.viewer = server.agent("claude")
        control = server.new_control_path()
        # The test server's clean environment, which has no TMUX either.
        env = {k: v for k, v in server.env.items() if k not in ("TMUX", "TMUX_PANE")}
        # Named after Claude Code's version, as the real one: not `claude`.
        self.process = subprocess.Popen(
            agent_argv(server.fakes["node"], control),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        wait_for_socket(control, lambda: "parked session")
        self.session = FakeAgent(server, "claude", self.viewer.pane, control, parent=None)

    def register(self, *, job: str = JOB, parked: str = JOB, viewer_start: str | None = None) -> None:
        sessions = self.config / "sessions"
        sessions.mkdir(parents=True, exist_ok=True)
        entries = [
            {"pid": self.session.pid, "procStart": procs.start_time(self.session.pid), "kind": "bg", "jobId": job},
            {
                "pid": self.viewer.pid,
                "procStart": viewer_start or procs.start_time(self.viewer.pid),
                "kind": "interactive",
                "parkedJobId": parked,
            },
        ]
        for entry in entries:
            (sessions / f"{entry['pid']}.json").write_text(json.dumps(entry))

    def hook(self, event: str, /, **fields: str) -> None:
        env = {"CLAUDE_CONFIG_DIR": str(self.config), "CLAUDE_CODE_SESSION_KIND": "bg"}
        self.session.hook(event, _env=env, **fields)

    def close(self) -> None:
        self.process.kill()
        self.process.wait()


@pytest.fixture
def parked(server: TmuxServer, tmp_path: Path) -> Iterator[Parked]:
    p = Parked(server, tmp_path / "claude")
    yield p
    p.close()


@rule("I5")
@change("C11")
def test_I5_a_parked_session_reports_to_the_pane_that_shows_it(parked: Parked) -> None:
    parked.register()
    parked.hook("UserPromptSubmit")
    assert state(parked.viewer) == "working"
    assert shown(parked.viewer)["@agent_session"] == parked.session.session_id
    parked.hook("Stop", last_assistant_message="Deployed.")
    assert state(parked.viewer) in ("done", "idle")
    assert shown(parked.viewer)["@agent_msg"] == "Deployed."


@rule("I5", "B1")
@change("C11")
def test_I5_its_shells_are_the_sessions(parked: Parked) -> None:
    parked.register()
    parked.hook("UserPromptSubmit")
    shell = background_shell(parked.session)
    parked.hook("Stop")
    wait_bg(parked.viewer, "1")
    parked.session.signal(shell)
    wait_bg(parked.viewer, "")


@rule("I5")
@pytest.mark.parametrize("case", ["no registry", "another job", "viewer pid reused", "not bg"])
def test_I5_nothing_without_a_viewer(parked: Parked, case: str) -> None:
    match case:
        case "another job":
            parked.register(parked="0000000a")
        case "viewer pid reused":
            parked.register(viewer_start="1")
        case "not bg":
            parked.register()
    if case == "not bg":
        env = {"CLAUDE_CONFIG_DIR": str(parked.config)}
        parked.session.hook("UserPromptSubmit", _env=env)
    else:
        parked.hook("UserPromptSubmit")
    assert state(parked.viewer) == ""
