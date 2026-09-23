"""Benchmarks of the agent layer (T0.7), on the same isolated harness as the
contract suite. Prints a Markdown report; `--json` writes the raw numbers.

    uv run python bench/run.py                     # bash
    AGENT_IMPL=rust AGENTD=... uv run python bench/run.py

1. Hook latency per event: wall time of one hook call as its agent sees it
   (process start to exit), measured inside the fake agent. Claude and Codex
   turns in a loop; alerts on (sounds and notifications go to the sink),
   every pane away. A short pause after each turn lets detached helpers
   finish, so they don't slow the next measure.
2. Resident processes: what the implementation leaves running (RSS and PSS)
   in a steady scene (four agents: waiting, done with a background shell,
   done, Codex working) and after 20 waits on one pane.
3. Blink: CPU of the implementation's processes and of the tmux server
   while one pane blinks as `needs`, and while nothing blinks.
"""

import argparse
import json
import shutil
import statistics
import sys
import tempfile
import time
from collections.abc import Callable, Generator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harness import procs  # noqa: E402
from harness.agent import FakeAgent, make_fakes  # noqa: E402
from harness.codex import Codex, Rollout  # noqa: E402
from harness.impl import IMPL  # noqa: E402
from harness.tmux import TmuxServer  # noqa: E402

TICK = 100  # clock ticks per second (getconf CLK_TCK)
AGENT_COMMS = ("claude", "codex", "node")
BG_SHELL = "source /x/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 600; exit 0"

type Samples = dict[str, list[float]]


@contextmanager
def scene(fakes: dict[str, Path], **options: Any) -> Generator[TmuxServer]:
    root = Path(tempfile.mkdtemp(prefix="agt-bench-", dir="/tmp"))
    server = TmuxServer(root, fakes, **options)
    try:
        yield server
    finally:
        server.kill()
        problems = server.tripped()
        shutil.rmtree(root, ignore_errors=True)
        if problems:
            raise RuntimeError(f"tripwire: {problems}")


# --- 1. latency ------------------------------------------------------------------


def timed(samples: Samples, key: str, run: Callable[[], Any]) -> None:
    result = run()
    samples.setdefault(key, []).append(result.ms)


def claude_turns(server: TmuxServer, turns: int) -> Samples:
    samples: Samples = {}
    agent = server.agent("claude")
    h = agent.hook
    timed(samples, "SessionStart", lambda: h("SessionStart", source="startup", model="m"))
    for i in range(turns):
        timed(samples, "UserPromptSubmit", lambda: h("UserPromptSubmit"))
        timed(samples, "PreToolUse", lambda: h("PreToolUse", tool_name="Bash", tool_use_id=f"a{i}",
                                               tool_input={"command": "ls -la"}))
        timed(samples, "PostToolUse", lambda: h("PostToolUse", tool_name="Bash", tool_use_id=f"a{i}"))
        timed(samples, "SubagentStart", lambda: h("SubagentStart", agent_id=f"s{i}", agent_type="Explore"))
        timed(samples, "SubagentStop", lambda: h("SubagentStop", agent_id=f"s{i}", agent_type="Explore"))
        h("PreToolUse", tool_name="Bash", tool_use_id=f"b{i}", tool_input={"command": "rm -r build"})
        timed(samples, "PermissionRequest", lambda: h("PermissionRequest", tool_name="Bash",
                                                      tool_use_id=f"b{i}",
                                                      tool_input={"command": "rm -r build"}))
        timed(samples, "PostToolUse (answer)", lambda: h("PostToolUse", tool_name="Bash", tool_use_id=f"b{i}"))
        timed(samples, "Notification", lambda: h("Notification", notification_type="idle_prompt"))
        timed(samples, "PreCompact", lambda: h("PreCompact"))
        timed(samples, "PostCompact", lambda: h("PostCompact"))
        timed(samples, "Stop", lambda: h("Stop", last_assistant_message="Done."))
        time.sleep(0.3)
    timed(samples, "SessionEnd", lambda: h("SessionEnd"))
    return samples


def codex_turns(server: TmuxServer, root: Path, turns: int) -> Samples:
    samples: Samples = {}
    c = Codex(server.agent("codex"), Rollout(root / "rollout.jsonl"))
    timed(samples, "SessionStart", lambda: c.hook("SessionStart", source="startup"))
    for i in range(turns):
        c.turn = f"t{i}"
        timed(samples, "UserPromptSubmit", lambda: c.hook("UserPromptSubmit"))
        c.rollout.started(c.turn)
        timed(samples, "PreToolUse", lambda: c.hook("PreToolUse", tool_name="Bash", tool_use_id=f"a{i}",
                                                    tool_input={"command": "ls"}))
        timed(samples, "PermissionRequest", lambda: c.hook("PermissionRequest", tool_name="Bash",
                                                           tool_input={"command": "ls"}))
        timed(samples, "PostToolUse", lambda: c.hook("PostToolUse", tool_name="Bash", tool_use_id=f"a{i}"))
        timed(samples, "Stop", lambda: c.hook("Stop", last_assistant_message="Done."))
        c.rollout.complete(c.turn)
        time.sleep(0.3)
    timed(samples, "SessionEnd", lambda: c.hook("SessionEnd"))
    return samples


def percentile(values: list[float], p: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(p / 100 * (len(ordered) - 1)))]


def latency_table(samples: Samples) -> list[dict[str, Any]]:
    return [
        {
            "event": event,
            "n": len(values),
            "p50": statistics.median(values),
            "p90": percentile(values, 90),
            "p99": percentile(values, 99),
            "max": max(values),
        }
        for event, values in samples.items()
    ]


# --- 2. resident processes ------------------------------------------------------------


def status_kb(pid: int, field: str, name: str = "status") -> int:
    try:
        for line in Path(f"/proc/{pid}/{name}").read_text().splitlines():
            if line.startswith(field + ":"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def resident(server: TmuxServer, agents: list[FakeAgent]) -> list[dict[str, Any]]:
    """The implementation's processes: everything under the server's marker
    except tmux itself, the panes' processes, the fake agents and what they
    started (hooks, background shells)."""
    ours: set[int] = {server.pid}
    panes = server.tmux("list-panes", "-a", "-F", "#{pane_pid}").split()
    ours |= {int(p) for p in panes}
    roots = ours | {a.pid for a in agents}
    rows: list[dict[str, Any]] = []
    for pid in procs.marked(server.marker):
        comm = procs.comm(pid) or "?"
        if comm.startswith("tmux") or any(a in roots for a in procs.ancestry(pid)):
            continue
        rows.append({"pid": pid, "comm": comm, "rss_kb": status_kb(pid, "VmRSS"),
                     "pss_kb": status_kb(pid, "Pss", "smaps_rollup")})
    return rows


def steady_scene(fakes: dict[str, Path]) -> list[dict[str, Any]]:
    with scene(fakes) as server:
        waiting, background, finished = (server.agent("claude") for _ in range(3))
        for agent in (waiting, background, finished):
            agent.hook("SessionStart", source="startup")
            agent.hook("UserPromptSubmit")
        waiting.hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})
        background.spawn(["/bin/bash", "-c", BG_SHELL])
        background.hook("Stop")
        finished.hook("Stop")
        c = Codex(server.agent("codex"), Rollout(server.root / "rollout.jsonl"))
        c.start()
        time.sleep(3)
        return resident(server, [waiting, background, finished, c.agent])


def waits_scene(fakes: dict[str, Path], waits: int = 20) -> list[dict[str, Any]]:
    with scene(fakes) as server:
        agent = server.agent("claude")
        agent.hook("SessionStart", source="startup")
        agent.hook("UserPromptSubmit")
        for i in range(waits):
            agent.hook("PermissionRequest", tool_name="Bash", tool_use_id=f"a{i}", tool_input={"command": "ls"})
            agent.hook("PostToolUse", tool_name="Bash", tool_use_id=f"a{i}")
        time.sleep(3)
        return resident(server, [agent])


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    by_comm: dict[str, int] = {}
    for row in rows:
        by_comm[row["comm"]] = by_comm.get(row["comm"], 0) + 1
    return {
        "processes": len(rows),
        "rss_mb": sum(r["rss_kb"] for r in rows) / 1024,
        "pss_mb": sum(r["pss_kb"] for r in rows) / 1024,
        "by_comm": by_comm,
    }


# --- 3. blink CPU ---------------------------------------------------------------------


def cpu_split(server: TmuxServer, agents: list[FakeAgent], seconds: float) -> dict[str, float]:
    """CPU % of one core over `seconds`: the implementation's processes
    (their reaped children included) and the tmux server."""
    def impl_pids() -> list[int]:
        return [r["pid"] for r in resident(server, agents)]

    before_impl = procs.cpu_ticks(impl_pids())
    before_tmux = procs.cpu_ticks([server.pid])
    time.sleep(seconds)
    after_impl = procs.cpu_ticks(impl_pids())
    after_tmux = procs.cpu_ticks([server.pid])
    impl = sum(t - before_impl.get(p, 0) for p, t in after_impl.items())
    tmux = sum(t - before_tmux.get(p, 0) for p, t in after_tmux.items())
    return {"impl_pct": 100 * impl / TICK / seconds, "tmux_pct": 100 * tmux / TICK / seconds}


def blink_cpu(fakes: dict[str, Path], seconds: float) -> dict[str, dict[str, float]]:
    with scene(fakes) as server:
        server.set_global("@agent_remind_after", "3600")
        agent = server.agent("claude")
        agent.hook("SessionStart", source="startup")
        agent.hook("UserPromptSubmit")
        agent.hook("PermissionRequest", tool_name="Bash", tool_use_id="a", tool_input={"command": "ls"})
        time.sleep(2)
        blinking = cpu_split(server, [agent], seconds)
        agent.hook("PostToolUse", tool_name="Bash", tool_use_id="a")
        time.sleep(3)
        quiet = cpu_split(server, [agent], min(seconds, 10))
        return {"needs blinking": blinking, "nothing blinking": quiet}


# --- report -----------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--turns", type=int, default=50, help="turns per agent kind (default 50)")
    parser.add_argument("--blink-seconds", type=float, default=30)
    parser.add_argument("--json", type=Path, help="also write the raw numbers here")
    args = parser.parse_args()

    fakes_dir = Path(tempfile.mkdtemp(prefix="agt-fakes-", dir="/tmp"))
    try:
        fakes = make_fakes(fakes_dir)
        with scene(fakes) as server:
            claude = latency_table(claude_turns(server, args.turns))
        with scene(fakes) as server:
            codex = latency_table(codex_turns(server, server.root, args.turns))
        steady = summarize(steady_scene(fakes))
        waits = summarize(waits_scene(fakes))
        blink = blink_cpu(fakes, args.blink_seconds)
    finally:
        shutil.rmtree(fakes_dir, ignore_errors=True)

    out = [f"## Benchmarks: {IMPL.name} ({time.strftime('%Y-%m-%d')})", ""]
    for kind, table in (("Claude", claude), ("Codex", codex)):
        out += [f"Hook latency, {kind} (ms, {args.turns} turns):", "",
                "| Event | n | p50 | p90 | p99 | max |", "|---|---|---|---|---|---|"]
        out += [f"| {r['event']} | {r['n']} | {r['p50']:.1f} | {r['p90']:.1f} | {r['p99']:.1f} | {r['max']:.1f} |"
                for r in table]
        out.append("")
    out += ["Resident processes of the implementation:", "",
            "| Scene | Processes | RSS MB | PSS MB | By name |", "|---|---|---|---|---|"]
    for name, s in (("steady (4 agents)", steady), ("after 20 waits on one pane", waits)):
        names = ", ".join(f"{c} ×{n}" for c, n in sorted(s["by_comm"].items()))
        out.append(f"| {name} | {s['processes']} | {s['rss_mb']:.1f} | {s['pss_mb']:.1f} | {names} |")
    out += ["", f"CPU, % of one core ({args.blink_seconds:.0f} s):", "",
            "| Scene | Implementation | tmux server |", "|---|---|---|"]
    out += [f"| {name} | {v['impl_pct']:.2f} | {v['tmux_pct']:.2f} |" for name, v in blink.items()]
    print("\n".join(out))
    if args.json:
        args.json.write_text(json.dumps(
            {"impl": IMPL.name, "claude": claude, "codex": codex, "steady": steady, "waits": waits,
             "blink": blink}, indent=2))


if __name__ == "__main__":
    main()
