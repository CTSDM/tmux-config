"""An isolated tmux server for one test, and everything that runs under it.

Safety (design.md, "Rules"): the server is `tmux -L <unique name>` started
without TMUX/TMUX_PANE, from a clean environment: temporary HOME and
XDG_RUNTIME_DIR, the AG_SINK and AG_FOCUS_CLIENT seams in the server's global
environment (tmux hooks run helpers with it), no D-Bus address, and
tripwires as the sound player and at the fallback bus path. Sounds stay on
and no space is muted: those switches are what the tests check. `kill()`
stops that server by name and every process that inherited its marker.
"""

import fcntl
import functools
import itertools
import os
import secrets
import struct
import subprocess
import termios
import threading
from pathlib import Path
from typing import Literal

from . import procs, tripwire
from .agent import FakeAgent, agent_argv, wait_for_socket
from .impl import IMPL, Impl
from .sink import Sink
from .wait import eventually

US = "\x1f"
SOUNDS = (
    "need-backup",
    "report-in",
    "wait-for-my-go",
    "lets-do-this",
    "enemy-down",
    "ct-win",
    "oh-man",
    "come-to-papa",
    "fight-like-a-man",
)
SPACE = "test"
_servers = itertools.count(1)

type Focus = Literal["none", "client", "flag"]


@functools.cache
def uv_dirs() -> dict[str, str]:
    """The real uv cache and interpreters, so agent-notify (a uv script)
    still starts under the temporary HOME."""
    found: dict[str, str] = {}
    for var, args in (("UV_CACHE_DIR", ["cache", "dir"]), ("UV_PYTHON_INSTALL_DIR", ["python", "dir"])):
        try:
            found[var] = subprocess.check_output(["uv", *args], text=True).strip()
        except (OSError, subprocess.CalledProcessError):
            pass
    return found


class TmuxServer:
    """
    focus: which tmux client has keyboard focus (contract V1).
      "none"    AG_FOCUS_CLIENT=none: every pane is away.
      "client"  a client attached to session `main`, named in AG_FOCUS_CLIENT.
      "flag"    that client, AG_FOCUS_CLIENT unset: tmux's focused flag decides
                (there is no Hyprland socket in the temporary runtime dir).
    The focus is fixed before any agent or daemon starts: they inherit it.
    conf: source the implementation's tmux configuration (IMPL.conf).
    """

    def __init__(
        self,
        root: Path,
        fakes: dict[str, Path],
        *,
        focus: Focus = "none",
        conf: bool = True,
        impl: Impl = IMPL,
    ) -> None:
        self.root = root
        self.fakes = fakes
        self.impl = impl
        self.name = f"agtest-{os.getpid()}-{next(_servers)}"
        self.marker = f"{self.name}-{secrets.token_hex(4)}"
        self.sink = Sink(root / "sink.jsonl")
        self.player_log = root / "tripwire-player.log"
        self.agents: list[FakeAgent] = []
        self.client: Client | None = None
        self._controls = itertools.count(1)
        self._killed = False

        for sub in ("home", "state", "data", "config", "cache", "sounds"):
            (root / sub).mkdir(parents=True, exist_ok=True)
        (root / "run").mkdir(mode=0o700, exist_ok=True)
        for sound in SOUNDS:
            (root / "sounds" / f"{sound}.wav").touch()
        (root / "spaces.conf").write_text(f"{SPACE} *\n")
        tripwire.player_script(root / "tripwire-player", self.player_log)
        # Where D-Bus clients look when DBUS_SESSION_BUS_ADDRESS is unset.
        self.bus = tripwire.Bus(root / "run" / "bus")

        keep = ("PATH", "USER", "LOGNAME", "TZ")
        self.env: dict[str, str] = {k: os.environ[k] for k in keep if k in os.environ}
        self.env.update(uv_dirs())
        self.env.update(
            {
                "HOME": str(root / "home"),
                "LANG": "C.UTF-8",
                "TERM": "xterm-256color",
                "SHELL": "/bin/sh",
                "XDG_RUNTIME_DIR": str(root / "run"),
                "XDG_STATE_HOME": str(root / "state"),
                "XDG_DATA_HOME": str(root / "data"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "AG_SINK": str(self.sink.path),
                "AG_FOCUS_CLIENT": "none",
                "AG_SOUNDS": str(root / "sounds"),
                "AG_SOUND_PLAYER": str(root / "tripwire-player"),
                "AG_SPACES_FILE": str(root / "spaces.conf"),
                "PYTHONDONTWRITEBYTECODE": "1",
                "UV_OFFLINE": "1",  # agent-notify starts from the cache or fails, never downloads
                procs.MARKER_VAR: self.marker,
            }
        )

        base = root / "base.conf"
        base.write_text(
            "\n".join(
                [
                    "set -g default-size 200x50",
                    "set -g focus-events on",
                    "set -wg remain-on-exit on",
                    # agents.conf sets it too; agentd finds the bash helpers by it (phase 1).
                    f"set -g @agents_bin '{impl.bin}'",
                    "",
                ]
            )
        )
        try:
            self.tmux("-f", str(base), "new-session", "-d", "-s", "main", "--", *IDLE)
            self.tmux("set", "-t", "main", "@space", SPACE)
            self.socket_path = self.tmux("display", "-p", "#{socket_path}")
            self.pid = int(self.tmux("display", "-p", "#{pid}"))
            if focus != "none":
                self.client = Client(self, "main")
                if focus == "client":
                    self._setenv("AG_FOCUS_CLIENT", self.client.name)
                else:
                    self._setenv("AG_FOCUS_CLIENT", None)
            if conf:
                self.tmux("source-file", str(impl.conf))
            # What tmux.conf does on load; after the focus is fixed, since the
            # daemon reads the seams from its environment once.
            self.ensure()
        except BaseException:
            self.kill()
            raise

    def __repr__(self) -> str:
        return f"TmuxServer(-L {self.name})"

    # --- running things ----------------------------------------------------

    def tmux(self, *args: str, check: bool = True) -> str:
        """`tmux -L <name> ...` with the test environment (never the live TMUX)."""
        done = subprocess.run(
            ["tmux", "-L", self.name, *args], env=self.env, capture_output=True, text=True
        )
        if check and done.returncode != 0:
            raise RuntimeError(f"tmux {' '.join(args)}: {done.stderr.strip()}")
        return done.stdout.removesuffix("\n")

    def tool_env(self, pane: str | None = None) -> dict[str, str]:
        """Environment for implementation tools run from the test process
        (reconcile, ctl): TMUX points at this server."""
        env = {**self.env, "TMUX": f"{self.socket_path},{self.pid},0"}
        if pane is not None:
            env["TMUX_PANE"] = pane
        return env

    def run_tool(self, argv: list[str], timeout: float = 30) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            argv, env=self.tool_env(), capture_output=True, text=True, timeout=timeout
        )

    def start_tool(self, argv: list[str]) -> None:
        """Start a command in the background, like `run-shell -b` (it carries
        the marker, so teardown stops it)."""
        subprocess.Popen(
            argv,
            env=self.tool_env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )

    def reconcile(self, *panes: str) -> None:
        self.run_tool(self.impl.reconcile_argv(*panes))

    def ensure(self) -> None:
        """What loading tmux.conf does to start the implementation (Rust only)."""
        argv = self.impl.ensure_argv()
        if argv is not None:
            self.run_tool(argv)

    def _setenv(self, name: str, value: str | None) -> None:
        """Global environment: inherited by panes (and so agents, hooks and
        the daemon) started from now on."""
        if value is None:
            self.tmux("set-environment", "-gu", name)
            self.env.pop(name, None)
        else:
            self.tmux("set-environment", "-g", name, value)
            self.env[name] = value

    # --- sessions and agents ---------------------------------------------------

    def session(self, name: str, *, space: str | None = SPACE, cwd: str | None = None) -> str:
        """A detached session with an idle pane; `space` sets its @space."""
        args = ["new-session", "-d", "-s", name]
        if cwd is not None:
            args += ["-c", cwd]
        self.tmux(*args, "--", *IDLE)
        if space is not None:
            self.tmux("set", "-t", name, "@space", space)
        return name

    def new_control_path(self) -> Path:
        return self.root / f"c{next(self._controls)}.sock"

    def agent(
        self,
        kind: str = "claude",
        session: str = "main",
        *,
        shell: bool = False,
        split: str | None = None,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
    ) -> FakeAgent:
        """A fake agent in a new pane: a new window of `session`, or a split
        of pane `split`. With `shell`, the pane runs a shell whose child is
        the agent (and which stays when the agent ends), as in real life;
        otherwise the agent is the pane's process itself."""
        control = self.new_control_path()
        argv = agent_argv(self.fakes[kind], control)
        if shell:
            argv = ["/bin/sh", "-c", '"$@"; read -r _', "sh", *argv]
        args = ["split-window", "-t", split] if split else ["new-window", "-t", f"{session}:"]
        args += ["-d", "-P", "-F", "#{pane_id}"]
        for key, value in (env or {}).items():
            args += ["-e", f"{key}={value}"]
        if cwd is not None:
            args += ["-c", cwd]
        pane = self.tmux(*args, "--", *argv)
        wait_for_socket(control, lambda: f"agent in {pane}: {self.capture(pane)!r}")
        agent = FakeAgent(self, kind, pane, control, parent=None)
        self.agents.append(agent)
        return agent

    def pane_pid(self, pane: str) -> int:
        return int(self.tmux("display", "-p", "-t", pane, "#{pane_pid}"))

    def capture(self, pane: str) -> str:
        return self.tmux("capture-pane", "-p", "-t", pane, check=False)

    # --- options -----------------------------------------------------------

    def _options(self, scope: list[str], target: str) -> dict[str, str]:
        listing = self.tmux("show-options", *scope, "-t", target)
        names = [line.split(" ", 1)[0] for line in listing.splitlines() if line.startswith("@")]
        if not names:
            return {}
        # show-options quotes values; the format gives them raw.
        raw = self.tmux("display", "-p", "-t", target, US.join("#{" + n + "}" for n in names))
        return dict(zip(names, raw.split(US), strict=True))

    def pane_options(self, pane: str) -> dict[str, str]:
        """Every user option set on the pane itself (raw values, `##` as stored)."""
        return self._options(["-p"], pane)

    def agent_options(self, pane: str) -> dict[str, str]:
        return {k: v for k, v in self.pane_options(pane).items() if k.startswith("@agent")}

    def window_options(self, window: str) -> dict[str, str]:
        return self._options(["-w"], window)

    def session_options(self, session: str) -> dict[str, str]:
        return self._options([], session)

    def option(self, pane: str, name: str) -> str:
        """A pane option's raw value, '' when unset."""
        return self.tmux("show-options", "-pqv", "-t", pane, name)

    def is_set(self, pane: str, name: str) -> bool:
        return name in self.pane_options(pane)

    def wait_option(self, pane: str, name: str, value: str, timeout: float = 5.0) -> None:
        eventually(
            lambda: self.option(pane, name),
            lambda v: v == value,
            timeout,
            f"{pane} {name} == {value!r}",
        )

    def global_option(self, name: str) -> str:
        return self.tmux("show-options", "-gqv", name)

    def set_global(self, name: str, value: str) -> None:
        self.tmux("set-option", "-g", name, value)

    def unset_global(self, name: str) -> None:
        self.tmux("set-option", "-gu", name)

    # --- tripwires ----------------------------------------------------------

    def tripped(self) -> list[str]:
        problems: list[str] = []
        if self.player_log.exists():
            problems.append(f"sound player called: {self.player_log.read_text().strip()}")
        if self.bus.hits:
            problems.append(f"D-Bus session bus used by: {self.bus.callers}")
        return problems

    # --- teardown ------------------------------------------------------------

    def kill(self) -> list[int]:
        """Kill this server by name, then every process carrying its marker.
        Returns the pids that were still running after the server went away."""
        if self._killed:
            return []
        self._killed = True
        if self.client is not None:
            self.client.close()
        subprocess.run(
            ["tmux", "-L", self.name, "kill-server"], env=self.env, capture_output=True
        )
        leftovers = procs.kill_marked(self.marker)
        self.bus.close()
        socket = Path(f"/tmp/tmux-{os.getuid()}") / self.name
        if socket.is_socket():
            socket.unlink()
        return leftovers


# The idle pane of a session: a shell waiting on its terminal, no children.
IDLE = ("/bin/sh", "-c", "read -r _")


class Client:
    """A real tmux client attached to the test server, on a pseudo-terminal
    owned by the test. Its name (the tty) is known before it attaches."""

    def __init__(self, server: TmuxServer, session: str, cols: int = 200, rows: int = 50) -> None:
        self.server = server
        self.master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.name = os.ttyname(slave)
        self.output = bytearray()
        # setsid -c: a session of its own with this tty as the controlling one.
        self.process = subprocess.Popen(
            ["setsid", "-c", "tmux", "-L", server.name, "attach", "-t", session],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=server.env,
        )
        os.close(slave)
        self._reader = threading.Thread(target=self._drain, daemon=True)
        self._reader.start()
        eventually(
            lambda: server.tmux("list-clients", "-F", "#{client_name}").splitlines(),
            lambda names: self.name in names,
            5,
            f"client {self.name} attached",
        )

    def _drain(self) -> None:
        """Read what tmux draws, so the client never blocks on a full pty."""
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            self.output += data
            del self.output[:-65536]

    def value(self, fmt: str) -> str:
        """A format evaluated for this client."""
        for line in self.server.tmux("list-clients", "-F", "#{client_name}" + US + fmt).splitlines():
            name, _, rest = line.partition(US)
            if name == self.name:
                return rest
        raise RuntimeError(f"client {self.name} is gone")

    def focus_in(self) -> None:
        """The terminal window gains keyboard focus (as the terminal reports it)."""
        os.write(self.master, b"\x1b[I")

    def focus_out(self) -> None:
        os.write(self.master, b"\x1b[O")

    def switch(self, target: str) -> None:
        self.server.tmux("switch-client", "-c", self.name, "-t", target)

    def close(self) -> None:
        if self.process.poll() is None:
            self.server.tmux("detach-client", "-t", self.name, check=False)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        try:
            os.close(self.master)
        except OSError:
            pass
