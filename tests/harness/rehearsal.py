"""The cutover rehearsal (T5.5): a real install of one commit of this repo
into the temporary HOME of an isolated test server.

The repo is cloned to ~/.config/tmux at the commit, `setup.sh` runs for
real (TPM, tmuxifier, agentd built with the machine's toolchain), and the
test server loads that ~/.config/tmux/tmux.conf. Safety: HOME and every XDG
dir are temporary; CLAUDE_CONFIG_DIR and CODEX_HOME are never set, so
agents/install writes only there; TMUX_TMPDIR is temporary too, because
setup.sh (TPM's install_plugins) runs a bare `tmux`, whose default socket
would otherwise be the live server's.
"""

import os
import subprocess
from pathlib import Path

from .impl import REPO, Impl
from .tmux import TmuxServer

CARGO_HOME = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
RUSTUP_HOME = Path(os.environ.get("RUSTUP_HOME", Path.home() / ".rustup"))


def clone(commit: str, dest: Path) -> Path:
    """This repo at `commit`, cloned (its own .git, so setup.sh's git config
    stays in the clone)."""
    subprocess.run(["git", "clone", "-q", "--no-hardlinks", str(REPO), str(dest)], check=True)
    subprocess.run(["git", "-C", str(dest), "checkout", "-q", commit], check=True)
    return dest


def impl_of(config: Path) -> Impl:
    """The bash implementation of an installed config (for @agents_bin)."""
    return Impl("bash", config / "agents" / "bin", None, config / "agents" / "agents.conf")


class Install:
    """The installed config and its switches, in `server`'s HOME."""

    def __init__(self, server: TmuxServer) -> None:
        self.server = server
        self.home = Path(server.env["HOME"])
        self.config = self.home / ".config" / "tmux"
        self.agentd = self.home / ".local" / "bin" / "agentd"
        self.off = Path(server.env["XDG_STATE_HOME"]) / "tmux-agents" / "agentd.off"
        self.tmux_tmpdir = server.root / "tmux-tmpdir"
        self.tmux_tmpdir.mkdir(mode=0o700, exist_ok=True)

    def env(self) -> dict[str, str]:
        """For setup.sh and the docs' commands: the server's clean env, no
        TMUX, a temporary default tmux socket, the real Rust toolchain."""
        return {
            **self.server.env,
            "TMUX_TMPDIR": str(self.tmux_tmpdir),
            "CARGO_HOME": str(CARGO_HOME),
            "RUSTUP_HOME": str(RUSTUP_HOME),
            "CARGO_NET_OFFLINE": "true",
        }

    def run(self, *argv: str | Path, tmux: bool = False) -> subprocess.CompletedProcess[str]:
        """A command as the user would run it (`tmux`: inside the test
        server, TMUX pointing at it)."""
        env = self.env()
        if tmux:
            env["TMUX"] = self.server.tool_env()["TMUX"]
        return subprocess.run([str(a) for a in argv], env=env, capture_output=True, text=True, timeout=600)

    def setup_sh(self) -> str:
        done = self.run(self.config / "setup.sh", "--no-layouts")
        # TPM's install_plugins may have started a server on the temporary
        # default socket: stop that one, by its path.
        socket = self.tmux_tmpdir / f"tmux-{os.getuid()}" / "default"
        if socket.exists():
            subprocess.run(["tmux", "-S", str(socket), "kill-server"], env=self.env(), capture_output=True)
        assert done.returncode == 0, f"setup.sh failed:\n{done.stdout}\n{done.stderr}"
        return done.stdout + done.stderr

    def reload(self, tpm: bool = True) -> None:
        """prefix R: the full tmux.conf again. Without `tpm` (before setup.sh
        installed it) the plugin manager's own line is the one allowed error."""
        done = subprocess.run(["tmux", "-L", self.server.name, "source-file", str(self.config / "tmux.conf")],
                              env=self.server.env, capture_output=True, text=True)
        errors = (done.stdout + done.stderr).strip()
        if not tpm:
            errors = "\n".join(e for e in errors.splitlines() if "plugins/tpm/tpm" not in e)
        assert done.returncode == 0 or not tpm, f"tmux.conf: {errors}"
        assert not errors, f"tmux.conf: {errors}"
