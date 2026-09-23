"""Reader for the AG_SINK file (design.md, "Test seams"): one JSON object
per line, e.g. {"t": 1790201509123, "effect": "sound", "name": "need-backup"}."""

import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

type Line = dict[str, Any]


class Sink:
    def __init__(self, path: Path) -> None:
        self.path = path

    def lines(self) -> list[Line]:
        """Complete lines so far (a line still being written is left out)."""
        try:
            text = self.path.read_text()
        except FileNotFoundError:
            return []
        return [json.loads(line) for line in text.split("\n")[:-1] if line.strip()]

    def sounds(self) -> list[str]:
        return [line["name"] for line in self.lines() if line.get("effect") == "sound"]

    def notifications(self, pane: str | None = None) -> list[Line]:
        return [
            line
            for line in self.lines()
            if line.get("effect") == "notify" and (pane is None or line.get("pane") == pane)
        ]

    def mark(self) -> int:
        """Position to pass to `since`, to look only at what comes after."""
        return len(self.lines())

    def since(self, mark: int) -> list[Line]:
        return self.lines()[mark:]

    def wait_for(
        self, predicate: Callable[[Line], bool], timeout: float = 5.0, after: int = 0
    ) -> Line:
        end = time.monotonic() + timeout
        while True:
            for line in self.lines()[after:]:
                if predicate(line):
                    return line
            if time.monotonic() >= end:
                raise AssertionError(
                    f"no matching sink line within {timeout}s; sink: {self.lines()[after:]}"
                )
            time.sleep(0.05)

    def wait_sound(self, name: str, timeout: float = 5.0, after: int = 0) -> Line:
        return self.wait_for(
            lambda line: line.get("effect") == "sound" and line.get("name") == name,
            timeout,
            after,
        )

    def quiet(self, seconds: float, after: int = 0) -> None:
        """Assert that nothing new reaches the sink for `seconds`."""
        time.sleep(seconds)
        extra = self.lines()[after:]
        assert not extra, f"unexpected sink lines: {extra}"
