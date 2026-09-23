"""Fake agents, seen from the test process."""

import itertools
import os
import json
import shutil
import signal
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from . import procs
from .impl import IMPL

if TYPE_CHECKING:
    from .tmux import TmuxServer

FAKE_AGENT = Path(__file__).with_name("fake_agent.py")
KINDS = ("claude", "codex")
_ids = itertools.count(1)

type Json = dict[str, Any]


def make_fakes(directory: Path) -> dict[str, Path]:
    """Copies of a Python interpreter named claude and codex: the kernel names
    a process after its executable, which is what /proc/<pid>/comm (and the
    ownership check) sees. A relocatable interpreter that needs its lib/ next
    to it cannot be copied; then a symlink, which names the process the same."""
    directory.mkdir(parents=True, exist_ok=True)
    candidates = [Path(sys.executable).resolve()]
    candidates += [(Path(d) / "python3").resolve() for d in os.get_exec_path() if (Path(d) / "python3").exists()]
    fakes: dict[str, Path] = {}
    for kind in KINDS:
        fakes[kind] = directory / kind
        for python in candidates:
            shutil.copy2(python, fakes[kind])
            if _runs(fakes[kind]):
                break
            fakes[kind].unlink()
        else:
            fakes[kind].symlink_to(candidates[0])
    return fakes


def _runs(python: Path) -> bool:
    check = "import json, sys; assert sys.version_info >= (3, 12)"
    return subprocess.run([python, "-I", "-c", check], capture_output=True).returncode == 0


def agent_argv(fake: Path, control: Path) -> list[str]:
    return [str(fake), "-I", "-B", str(FAKE_AGENT), "--control", str(control)]


@dataclass(frozen=True)
class HookResult:
    rc: int | None
    stdout: str
    stderr: str
    ns: int
    timeout: bool

    @property
    def ms(self) -> float:
        return self.ns / 1e6


class FakeAgent:
    """A fake `claude` or `codex` process in a pane of the test server."""

    def __init__(
        self, server: "TmuxServer", kind: str, pane: str, control: Path, parent: "FakeAgent | None"
    ) -> None:
        self.server = server
        self.kind = kind
        self.pane = pane
        self.control = control
        self.parent = parent
        self.session_id = f"test-session-{next(_ids)}"
        info = self.request("ping")
        self.pid: int = info["pid"]
        self.start_time = procs.start_time(self.pid)

    def __repr__(self) -> str:
        return f"FakeAgent({self.kind}, pane={self.pane}, pid={self.pid})"

    # --- transport -----------------------------------------------------------

    def request(self, op: str, timeout: float = 15.0, **fields: Any) -> Json:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(timeout)
            conn.connect(str(self.control))
            conn.sendall(json.dumps({"op": op, **fields}).encode() + b"\n")
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
        reply: Json = json.loads(data)
        if "error" in reply:
            raise RuntimeError(f"{self}: {op}: {reply['error']}")
        return reply

    # --- hooks ---------------------------------------------------------------

    def run_hook(
        self,
        stdin: str,
        *,
        argv: list[str] | None = None,
        kind: str | None = None,
        env: dict[str, str | None] | None = None,
        wrap: int = 0,
        timeout: float = 10.0,
        check: bool = True,
    ) -> HookResult:
        """Run the implementation's hook as a child of this agent (through
        `wrap` intermediate shells), with `stdin` as the payload. `env`
        overrides the agent's environment (None unsets). With `check`, fails
        unless contract I2 holds: exit 0, nothing on stdout, no timeout."""
        reply = self.request(
            "hook",
            timeout=timeout + 5,
            argv=argv or IMPL.hook_argv(kind or self.kind),
            stdin=stdin,
            env=env or {},
            wrap=wrap,
        )
        result = HookResult(
            rc=reply["rc"],
            stdout=reply["stdout"],
            stderr=reply["stderr"],
            ns=reply["ns"],
            timeout=reply["timeout"],
        )
        if check:
            assert not result.timeout, f"I2: hook did not return within {timeout}s: {result}"
            assert result.rc == 0, f"I2: hook exited {result.rc}: {result.stderr}"
            assert result.stdout == "", f"I2: hook wrote to stdout: {result.stdout!r}"
        return result

    def hook(self, event: str, /, **fields: Any) -> HookResult:
        """Send `event` with this agent's session id; keyword arguments are
        payload fields, except the run_hook options prefixed with `_`
        (_kind, _env, _wrap, _timeout, _check)."""
        options = {k[1:]: fields.pop(k) for k in list(fields) if k.startswith("_")}
        payload = {"hook_event_name": event, "session_id": self.session_id, **fields}
        return self.run_hook(json.dumps(payload), **options)

    # --- other processes -----------------------------------------------------

    def spawn(
        self, argv: list[str], *, env: dict[str, str | None] | None = None, new_session: bool = False
    ) -> int:
        """Start a child of this agent (stdio on /dev/null); returns its pid."""
        return int(self.request("spawn", argv=argv, env=env or {}, new_session=new_session)["pid"])

    def signal(self, pid: int, sig: int = signal.SIGTERM, *, group: bool = False) -> bool:
        return bool(self.request("signal", pid=pid, sig=int(sig), group=group)["sent"])

    def wait(self, pid: int, timeout: float = 5.0) -> int | None:
        return self.request("wait", timeout=timeout + 5, pid=pid)["rc"]

    def child_agent(self, kind: str = "claude") -> "FakeAgent":
        """Another fake agent started by this one, e.g. a `claude -p` run from
        a tool call: same pane, one more agent in the process chain."""
        control = self.server.new_control_path()
        self.spawn(agent_argv(self.server.fakes[kind], control))
        wait_for_socket(control, lambda: f"child agent of {self}")
        return FakeAgent(self.server, kind, self.pane, control, parent=self)

    def exit(self, code: int = 0) -> None:
        """The agent process ends (no SessionEnd)."""
        self.request("exit", code=code)
        end = time.monotonic() + 5
        while procs.alive(self.pid) and time.monotonic() < end:
            time.sleep(0.02)

    # --- what it published ---------------------------------------------------

    def options(self) -> dict[str, str]:
        return self.server.agent_options(self.pane)

    def option(self, name: str) -> str:
        return self.server.option(self.pane, name)


def wait_for_socket(path: Path, describe: Any, timeout: float = 10.0) -> None:
    end = time.monotonic() + timeout
    while not path.exists():
        if time.monotonic() >= end:
            raise RuntimeError(f"{describe()}: control socket never appeared")
        time.sleep(0.02)
