"""Where the hook latency tail of agentd comes from (Rust only).

    AGENT_IMPL=rust AGENTD=... uv run python bench/tail.py --turns 300

Three scenes, each `--turns` × (UserPromptSubmit, PreToolUse):
  claude alone            no Codex pane, so no observation tick
  codex                   the measured Codex agent's own turn is open: its
                          observation ticks every ~2 s
  claude + codex watched  a Claude agent while a Codex pane in the same
                          server has an open turn

A thread polls the agentd process every ~0.2 ms: its children (the `tmux`
it spawns) and whether one of its threads is running (state R). For the
hooks over the threshold it reports how many met background work of the
daemon (begun while no hook ran), and when the background CPU bursts came.

Then, with no hooks at all, what the daemon reads and spends per 2 s tick
(/proc/<agentd>/io, 10 s): with only Claude, and with a Codex turn open.
"""

import argparse
import os
import random
import shutil
import statistics
import sys
import tempfile
import threading
import time
from collections.abc import Callable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harness import procs  # noqa: E402
from harness.agent import FakeAgent, make_fakes  # noqa: E402
from harness.codex import Codex, Rollout  # noqa: E402
from harness.impl import IMPL  # noqa: E402
from harness.tmux import TmuxServer  # noqa: E402


class Busy:
    """Intervals (monotonic) during which agentd had a child process
    (`intervals`), and during which one of its threads was running on a CPU
    (`running`, state R in /proc/<pid>/task/*/stat)."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.intervals: list[tuple[float, float]] = []
        self.running: list[tuple[float, float]] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._poll, daemon=True)
        self._thread.start()

    def _children(self) -> bool:
        try:
            for task in Path(f"/proc/{self.pid}/task").iterdir():
                if (task / "children").read_text().strip():
                    return True
        except OSError:
            pass
        return False

    def _running(self) -> bool:
        try:
            for task in Path(f"/proc/{self.pid}/task").iterdir():
                if (task / "stat").read_text().rsplit(") ", 1)[1].startswith("R"):
                    return True
        except (OSError, IndexError):
            pass
        return False

    def _poll(self) -> None:
        since: float | None = None
        run_since: float | None = None
        while not self._stop.is_set():
            now = time.monotonic()
            if self._children():
                since = now if since is None else since
            elif since is not None:
                self.intervals.append((since, now))
                since = None
            if self._running():
                run_since = now if run_since is None else run_since
            elif run_since is not None:
                self.running.append((run_since, now))
                run_since = None
            time.sleep(0.0002)

    def stop(self) -> None:
        self._stop.set()
        self._thread.join()

    def at(self, t: float, before: float = 0.002) -> bool:
        """Was the daemon busy when a hook started at t (a child that
        appeared a little before it, so not the hook's own)?"""
        return any(start <= t - before and end >= t for start, end in self.intervals)


type Sample = tuple[str, float, float]  # event, start (monotonic), ms


def run(samples: list[Sample], event: str, call: Callable[[], object]) -> None:
    time.sleep(random.uniform(0.005, 0.05))  # no fixed phase against a 2 s tick
    start = time.monotonic()
    result = call()
    samples.append((event, start, getattr(result, "ms")))


def claude_loop(agent: FakeAgent, turns: int, samples: list[Sample]) -> None:
    agent.hook("SessionStart", source="startup")
    for i in range(turns):
        run(samples, "UserPromptSubmit", lambda: agent.hook("UserPromptSubmit"))
        run(samples, "PreToolUse", lambda: agent.hook("PreToolUse", tool_name="Bash", tool_use_id=f"a{i}",
                                                      tool_input={"command": "ls"}))


def codex_loop(c: Codex, turns: int, samples: list[Sample]) -> None:
    c.hook("SessionStart", source="startup")
    for i in range(turns):
        c.turn = f"t{i}"
        run(samples, "UserPromptSubmit", lambda: c.hook("UserPromptSubmit"))
        c.rollout.started(c.turn)
        run(samples, "PreToolUse", lambda: c.hook("PreToolUse", tool_name="Bash", tool_use_id=f"a{i}",
                                                  tool_input={"command": "ls"}))


def agentd_pid(server: TmuxServer) -> int:
    for pid in procs.marked(server.marker):
        if procs.comm(pid) == "agentd" and "daemon" in Path(f"/proc/{pid}/cmdline").read_bytes().decode():
            return pid
    raise RuntimeError("no agentd daemon under this server")


def scene(fakes: dict[str, Path], name: str, turns: int, threshold: float) -> list[str]:
    root = Path(tempfile.mkdtemp(prefix="agt-tail-", dir="/tmp"))
    server = TmuxServer(root, fakes)
    samples: list[Sample] = []
    try:
        if name == "codex":
            c = Codex(server.agent("codex"), Rollout(root / "rollout.jsonl"))
            c.hook("SessionStart", source="startup")  # starts the daemon
            busy = Busy(agentd_pid(server))
            codex_loop(c, turns, samples)
        else:
            agent = server.agent("claude")
            if name == "claude + codex watched":
                watched = Codex(server.agent("codex"), Rollout(root / "rollout.jsonl"))
                watched.start()  # an open turn: observed every ~2 s
            agent.hook("SessionStart", source="startup")
            busy = Busy(agentd_pid(server))
            claude_loop(agent, turns, samples)
        busy.stop()
    finally:
        server.kill()
        shutil.rmtree(root, ignore_errors=True)

    # Background work: busy intervals that began while no hook was running
    # (they may run on into one: that is the competition looked for).
    hooks = [(t, t + ms / 1000) for _, t, ms in samples]
    background = [
        (a, b) for a, b in busy.intervals if not any(ha - 0.001 <= a <= hb for ha, hb in hooks)
    ]
    # CPU work begun while no hook ran, long enough to matter (> 2 ms).
    cpu = [
        (a, b) for a, b in busy.running
        if b - a > 0.002 and not any(ha - 0.001 <= a <= hb for ha, hb in hooks)
    ]

    def overlaps_cpu(t: float, ms: float) -> bool:
        return any(a <= t + ms / 1000 and b >= t - 0.005 for a, b in cpu)

    def overlaps_background(t: float, ms: float) -> bool:
        return any(a <= t + ms / 1000 and b >= t - 0.005 for a, b in background)

    lines: list[str] = []
    for event in ("UserPromptSubmit", "PreToolUse"):
        mine = [(t, ms) for e, t, ms in samples if e == event]
        values = sorted(ms for _, ms in mine)
        slow = [(t, ms) for t, ms in mine if ms > threshold]
        busy_all = sum(busy.at(t) for t, _ in mine)
        busy_slow = sum(busy.at(t) for t, _ in slow)
        with_background = sum(overlaps_background(t, ms) for t, ms in slow)
        with_cpu = sum(overlaps_cpu(t, ms) for t, ms in slow)
        p99 = values[min(len(values) - 1, round(0.99 * (len(values) - 1)))]
        lines.append(
            f"| {name} | {event} | {len(values)} | {statistics.median(values):.1f} | {p99:.1f} | "
            f"{values[-1]:.1f} | {len(slow)} | {busy_slow} | {with_background} | {with_cpu} | "
            f"{100 * busy_all / len(values):.1f}% |"
        )
    for event in ("UserPromptSubmit", "PreToolUse"):
        mine = [ms for e, _, ms in samples if e == event]
        at = ", ".join(f"#{i} {ms:.0f}" for i, ms in enumerate(mine) if ms > threshold)
        if at:
            lines.append(f"- {name}, {event} over {threshold:g} ms at turn: {at}")
    span = samples[-1][1] - samples[0][1]
    lengths = sorted((b - a) * 1000 for a, b in background)
    typical = f", median {statistics.median(lengths):.1f} ms" if lengths else ""
    lines.append(f"- {name}: {len(background)} background busy intervals in {span:.0f} s{typical}")
    cpu_lengths = sorted((b - a) * 1000 for a, b in cpu)
    if cpu_lengths:
        starts = [a for a, _ in cpu]
        gaps = sorted(b - a for a, b in zip(starts, starts[1:]))
        period = f", every {statistics.median(gaps):.2f} s (median gap)" if gaps else ""
        lines.append(f"- {name}: {len(cpu)} background CPU bursts over 2 ms, median "
                     f"{statistics.median(cpu_lengths):.1f} ms, max {cpu_lengths[-1]:.1f} ms{period}")
    else:
        lines.append(f"- {name}: no background CPU bursts over 2 ms")
    return lines


def tick_cost(fakes: dict[str, Path], seconds: float = 10) -> list[str]:
    """Reads and CPU of the daemon per 2 s with no hooks, without and with
    a Codex observation."""
    def io(pid: int) -> tuple[int, int]:
        fields = dict(line.split(": ") for line in Path(f"/proc/{pid}/io").read_text().splitlines())
        return int(fields["syscr"]), int(fields["rchar"])

    def cpu(pid: int) -> int:
        fields = procs.stat(pid) or []
        return int(fields[11]) + int(fields[12]) if fields else 0

    rows: list[str] = []
    root = Path(tempfile.mkdtemp(prefix="agt-tail-", dir="/tmp"))
    server = TmuxServer(root, fakes)
    try:
        for name in ("Claude only (no observation)", "a Codex turn open"):
            if name.startswith("Claude"):
                agent = server.agent("claude")
                agent.hook("SessionStart", source="startup")
                agent.hook("UserPromptSubmit")
            else:
                Codex(server.agent("codex"), Rollout(root / "rollout.jsonl")).start()
            time.sleep(1)
            pid = agentd_pid(server)
            (r0, b0), c0, t0 = io(pid), cpu(pid), time.monotonic()
            time.sleep(seconds)
            (r1, b1), c1, t1 = io(pid), cpu(pid), time.monotonic()
            per = 2 / (t1 - t0)
            processes = sum(1 for e in Path("/proc").iterdir() if e.name.isdigit())
            rows.append(f"| {name} | {(r1 - r0) * per:.0f} | {(b1 - b0) * per / 1024:.0f} KiB | "
                        f"{(c1 - c0) * per * 1000 / TICKS:.0f} ms | {processes} |")
    finally:
        server.kill()
        shutil.rmtree(root, ignore_errors=True)
    return rows


TICKS = os.sysconf("SC_CLK_TCK")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--turns", type=int, default=300)
    parser.add_argument("--threshold", type=float, default=15.0, help="ms (default 15, the p99 target)")
    args = parser.parse_args()
    if IMPL.name != "rust":
        sys.exit("bench/tail.py measures agentd: set AGENT_IMPL=rust and AGENTD")
    load = os.getloadavg()[0]
    fakes_dir = Path(tempfile.mkdtemp(prefix="agt-fakes-", dir="/tmp"))
    try:
        fakes = make_fakes(fakes_dir)
        rows: list[str] = []
        for name in ("claude alone", "codex", "claude + codex watched"):
            rows += scene(fakes, name, args.turns, args.threshold)
        cost = tick_cost(fakes)
    finally:
        shutil.rmtree(fakes_dir, ignore_errors=True)
    print(f"Load average (1 min) {load:.1f} before, {os.getloadavg()[0]:.1f} after.\n")
    print(f"| Scene | Event | n | p50 | p99 | max | > {args.threshold:g} ms | of them, daemon busy at start "
          "| of them, background child during | of them, background CPU during "
          "| all hooks, daemon busy at start |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    print("\n".join(r for r in rows if r.startswith("|")))
    print("\nBackground work (the daemon started a child while no hook ran), and when the slow hooks came:\n")
    print("\n".join(r for r in rows if r.startswith("- ")))
    print("\nThe daemon per 2 s with no hooks (/proc/<agentd>/io, 10 s):\n")
    print("| Scene | Read syscalls | Read | CPU | Processes on the machine |\n|---|---|---|---|---|")
    print("\n".join(cost))


if __name__ == "__main__":
    main()
