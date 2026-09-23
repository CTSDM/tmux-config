"""§2 Pane options: what is not part of the contract (CHANGE C3)."""

import signal
from pathlib import Path

from harness.claude import ask, working
from harness.codex import Codex, Rollout
from harness.marks import change, rule
from harness.tmux import TmuxServer
from harness.wait import eventually

INTERNAL = ("@agent_bg_watch", "@agent_codex_watch", "@agent_notify_id", "@agent_notify_pid")


@rule("C3")
@change("C3")
def test_C3_no_watcher_or_notification_options(server: TmuxServer, tmp_path: Path) -> None:
    """Notifications, a Claude shell watch and a Codex observation leave no
    trace in the pane options."""
    claude = server.agent("claude")
    working(claude)
    shell = claude.spawn(["/bin/bash", "-c", "source /x/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 300; exit 0"])
    ask(claude, "Bash", "a")
    claude.hook("Stop")
    eventually(lambda: claude.option("@agent_bg"), lambda v: v == "1", 5, "@agent_bg")
    codex = Codex(server.agent("codex"), Rollout(tmp_path / "rollout.jsonl"))
    codex.start()
    server.sink.wait_for(lambda line: line["effect"] == "notify" and line["pane"] == claude.pane)
    seen: set[str] = set()
    for _ in range(10):
        for pane in (claude.pane, codex.pane):
            seen |= set(server.pane_options(pane)) & set(INTERNAL)
    claude.signal(shell, signal.SIGTERM)
    assert seen == set()
