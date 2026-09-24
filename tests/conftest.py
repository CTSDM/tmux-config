"""Fixtures of the contract suite. See README.md."""

import os
import shutil
import subprocess
import tempfile
from collections import defaultdict
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any, cast

import pytest

from harness.agent import make_fakes
from harness.impl import IMPL
from harness.tmux import TMUX_BIN, TmuxServer

# Unix socket paths are short (108 bytes): keep the roots near /.
TMP_BASE = os.environ.get("AG_TEST_TMPDIR", "/tmp")
KEEP = os.environ.get("AG_TEST_KEEP") == "1"  # keep temp dirs for debugging


def pytest_report_header() -> list[str]:
    tmux = str(TMUX_BIN or shutil.which("tmux"))
    version = subprocess.run([tmux, "-V"], capture_output=True, text=True).stdout.strip()
    return [f"agent implementation: {IMPL.name} ({IMPL.agentd or IMPL.bin})", f"tmux: {version} ({tmux})"]


@pytest.fixture(scope="session")
def fakes() -> Iterator[dict[str, Path]]:
    directory = Path(tempfile.mkdtemp(prefix="agt-fakes-", dir=TMP_BASE))
    yield make_fakes(directory)
    shutil.rmtree(directory, ignore_errors=True)


@pytest.fixture
def make_server(fakes: dict[str, Path]) -> Iterator[Callable[..., TmuxServer]]:
    """Factory: make_server(focus=..., conf=...) -> TmuxServer, killed after
    the test. The test fails if anything tried a real sound or notification."""
    servers: list[TmuxServer] = []

    def make(**options: Any) -> TmuxServer:
        root = Path(tempfile.mkdtemp(prefix="agt-", dir=TMP_BASE))
        server = TmuxServer(root, fakes, **options)
        servers.append(server)
        return server

    yield make
    problems: list[str] = []
    for server in servers:
        server.kill()
        problems += [f"{server}: {p}" for p in server.tripped()]
        if not KEEP:
            shutil.rmtree(server.root, ignore_errors=True)
    if problems:
        pytest.fail("tripwire: " + "; ".join(problems))


@pytest.fixture
def server(make_server: Callable[..., TmuxServer]) -> TmuxServer:
    return make_server()


# --- coverage by contract rule ------------------------------------------------

# Rule ids travel on each report, so the summary also works under xdist.
_outcomes: dict[str, tuple[list[str], str]] = {}


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    for item in items:
        ids = [str(i) for mark in item.iter_markers("rule") for i in mark.args]
        if ids:
            item.user_properties.append(("rules", ids))


def pytest_runtest_logreport(report: pytest.TestReport) -> None:
    ids = [str(i) for k, v in report.user_properties if k == "rules" for i in cast(list[str], v)]
    if not ids or not (report.when == "call" or report.outcome != "passed"):
        return
    outcome = "xfailed" if hasattr(report, "wasxfail") and report.outcome == "skipped" else report.outcome
    if hasattr(report, "wasxfail") and report.outcome == "passed":
        outcome = "xpassed"
    previous = _outcomes.get(report.nodeid)
    if previous is None or previous[1] == "passed":
        _outcomes[report.nodeid] = (ids, outcome)


def pytest_terminal_summary(terminalreporter: Any) -> None:
    by_rule: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    for ids, outcome in _outcomes.values():
        for rule_id in ids:
            by_rule[rule_id][outcome] += 1
    if not by_rule:
        return
    terminalreporter.section(f"contract rules ({IMPL.name})")
    for rule_id in sorted(by_rule, key=_rule_key):
        counts = ", ".join(f"{n} {o}" for o, n in sorted(by_rule[rule_id].items()))
        terminalreporter.write_line(f"{rule_id:6} {counts}")


def _rule_key(rule_id: str) -> tuple[str, int, str]:
    head = rule_id.rstrip("abcdefghijklmnopqrstuvwxyz")
    letters = head.rstrip("0123456789")
    number = head[len(letters) :]
    return (letters, int(number) if number else 0, rule_id[len(head) :])
