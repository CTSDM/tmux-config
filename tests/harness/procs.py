"""Processes: /proc readers and cleanup by marker.

Everything a test starts (tmux server, panes, fake agents, hooks, helpers the
hooks detach, the daemon) inherits a unique `AG_TEST_RUN=<marker>` from the
test server's environment. Cleanup kills exactly the processes carrying it,
by pidfd, never by command-line pattern (design.md, rule 4).
"""

import os
import signal
import time
from pathlib import Path

MARKER_VAR = "AG_TEST_RUN"


def comm(pid: int) -> str | None:
    try:
        return Path(f"/proc/{pid}/comm").read_text().strip()
    except OSError:
        return None


def stat(pid: int) -> list[str] | None:
    """Fields of /proc/<pid>/stat after the command name (state is [0])."""
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    except (OSError, IndexError):
        return None


def ppid(pid: int) -> int | None:
    fields = stat(pid)
    return int(fields[1]) if fields else None


def start_time(pid: int) -> str | None:
    fields = stat(pid)
    return fields[19] if fields else None


def alive(pid: int) -> bool:
    fields = stat(pid)
    return fields is not None and fields[0] != "Z"


def ancestry(pid: int, limit: int = 32) -> list[int]:
    """pid, its parent, and so on up to (not including) pid 1."""
    chain: list[int] = []
    current: int | None = pid
    while current is not None and current > 1 and len(chain) < limit:
        chain.append(current)
        current = ppid(current)
    return chain


def marked(marker: str) -> list[int]:
    """Live processes whose environment carries the marker."""
    needle = f"{MARKER_VAR}={marker}".encode()
    found: list[int] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        if needle in environ.split(b"\0") and alive(int(entry.name)):
            found.append(int(entry.name))
    return found


def signal_marked(pid: int, marker: str, sig: int) -> None:
    """Signal after re-checking the marker, through a pidfd when this Python
    has one, so a recycled pid is never hit."""
    needle = f"{MARKER_VAR}={marker}".encode()
    if not hasattr(os, "pidfd_open"):
        # Without pidfds: the marker and the start time must not change.
        started = start_time(pid)
        try:
            ok = needle in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
            if ok and started is not None and start_time(pid) == started:
                os.kill(pid, sig)
        except OSError:
            pass
        return
    try:
        fd = os.pidfd_open(pid)
    except OSError:
        return
    try:
        if needle in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"):
            signal.pidfd_send_signal(fd, sig)
    except OSError:
        pass
    finally:
        os.close(fd)


def kill_marked(marker: str, grace: float = 1.0) -> list[int]:
    """SIGTERM every marked process, SIGKILL what is left after `grace`.
    Repeats while new ones show up (a dying helper may start another).
    Returns the pids that had to be killed."""
    killed: list[int] = []
    for _ in range(5):
        pids = marked(marker)
        if not pids:
            break
        killed += pids
        for pid in pids:
            signal_marked(pid, marker, signal.SIGTERM)
        end = time.monotonic() + grace
        while time.monotonic() < end and any(alive(p) for p in pids):
            time.sleep(0.05)
        for pid in pids:
            if alive(pid):
                signal_marked(pid, marker, signal.SIGKILL)
    return killed


def cpu_ticks(pids: list[int]) -> dict[int, int]:
    """utime + stime + cutime + cstime of each process, in clock ticks."""
    ticks: dict[int, int] = {}
    for pid in pids:
        fields = stat(pid)
        if fields is not None:
            ticks[pid] = sum(int(f) for f in fields[11:15])
    return ticks


def cpu_ns(pids: list[int]) -> dict[int, int]:
    """CPU time of each process in ns: its threads (schedstat), plus its
    reaped children (cutime + cstime, in clock ticks)."""
    tick_ns = 1_000_000_000 // os.sysconf("SC_CLK_TCK")
    cpu: dict[int, int] = {}
    for pid in pids:
        try:
            threads = sum(int((t / "schedstat").read_text().split()[0])
                          for t in Path(f"/proc/{pid}/task").iterdir())
        except (OSError, ValueError, IndexError):
            continue
        fields = stat(pid)
        children = (int(fields[13]) + int(fields[14])) * tick_ns if fields else 0
        cpu[pid] = threads + children
    return cpu


class ChildWatch:
    """The children a process starts while this watches it: a thread polls
    /proc/<pid>/task/*/children as fast as it can. Children there before
    `start` (e.g. a tmux server's panes) do not count."""

    def __init__(self, pid: int) -> None:
        import threading

        self.pid = pid
        self.before = self._children()
        self.new: dict[int, str] = {}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._poll, daemon=True)

    def _children(self) -> set[int]:
        found: set[int] = set()
        try:
            for task in Path(f"/proc/{self.pid}/task").iterdir():
                found.update(int(c) for c in (task / "children").read_text().split())
        except OSError:
            pass
        return found

    def _poll(self) -> None:
        while not self._stop.is_set():
            for child in self._children() - self.before:
                if child not in self.new:
                    self.new[child] = comm(child) or "?"

    def __enter__(self) -> "ChildWatch":
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._stop.set()
        self._thread.join()
