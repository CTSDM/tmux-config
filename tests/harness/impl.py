"""Which implementation the suite drives.

    AGENT_IMPL=bash   (default) agents/bin/agent-hook of this worktree, or of
                      AGENT_BASH_BIN (e.g. another worktree's agents/bin)
    AGENT_IMPL=rust   the agentd binary at AGENTD

AGENT_CONF overrides the tmux-side configuration sourced into the test
server (default: agents/agents.conf next to the bash bin directory).
"""

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

REPO = Path(__file__).resolve().parents[2]

type ImplName = Literal["bash", "rust"]


@dataclass(frozen=True)
class Impl:
    name: ImplName
    bin: Path  # bash helpers; also the tmux config's @agents_bin
    agentd: Path | None
    conf: Path

    @property
    def theme(self) -> Path:
        """theme.conf of the same checkout: the bar (§15)."""
        return self.bin.parent.parent / "theme.conf"

    def hook_argv(self, kind: str) -> list[str]:
        if self.agentd is not None:
            return [str(self.agentd), "hook", kind]
        return [str(self.bin / "agent-hook"), kind]

    def reconcile_argv(self, *panes: str) -> list[str]:
        if self.agentd is not None:
            return [str(self.agentd), "ctl", "reconcile", *panes]
        return [str(self.bin / "agent-reconcile"), *panes]

    def blink_demo_argv(self, session: str, window: str, seconds: int) -> list[str]:
        """What prefix+Q "Preview the turn signal" runs (contract K1 demo)."""
        if self.agentd is not None:
            return [str(self.agentd), "ctl", "blink-demo", session, window, str(seconds)]
        return [str(self.bin / "agent-blink"), "--demo", session, window, str(seconds)]

    def ensure_argv(self) -> list[str] | None:
        """What tmux.conf runs on load to start the implementation, if anything."""
        return [str(self.agentd), "ensure"] if self.agentd is not None else None


def from_env() -> Impl:
    name = os.environ.get("AGENT_IMPL", "bash")
    bin_dir = Path(os.environ.get("AGENT_BASH_BIN", REPO / "agents" / "bin")).resolve()
    conf = Path(os.environ.get("AGENT_CONF", bin_dir.parent / "agents.conf")).resolve()
    if name == "bash":
        return Impl("bash", bin_dir, None, conf)
    if name == "rust":
        agentd = os.environ.get("AGENTD")
        if not agentd or not os.access(agentd, os.X_OK):
            raise RuntimeError("AGENT_IMPL=rust needs AGENTD=<path to the agentd binary>")
        return Impl("rust", bin_dir, Path(agentd).resolve(), conf)
    raise RuntimeError(f"AGENT_IMPL must be bash or rust, not {name!r}")


IMPL = from_env()
