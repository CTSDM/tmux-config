import time
from collections.abc import Callable


def eventually[T](
    probe: Callable[[], T],
    until: Callable[[T], bool],
    timeout: float = 5.0,
    what: str = "condition",
    interval: float = 0.05,
) -> T:
    """Poll `probe` until `until(value)`; return the value or fail with the last one."""
    end = time.monotonic() + timeout
    while True:
        value = probe()
        if until(value):
            return value
        if time.monotonic() >= end:
            raise AssertionError(f"{what}: not met within {timeout}s, last value {value!r}")
        time.sleep(interval)
