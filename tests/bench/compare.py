"""Side by side: two reports of bench/run.py (e.g. bash, then Rust, run back
to back on the same machine state) as one Markdown table of p50 / p99.

    uv run python bench/compare.py bash.md rust.md
"""

import re
import sys
from pathlib import Path

ROW = re.compile(r"\| (.+?) \| (\d+) \| ([\d.]+) \| ([\d.]+) \| ([\d.]+) \| ([\d.]+) \|")

type Latencies = dict[tuple[str, str], tuple[str, str]]


def read(path: Path) -> tuple[str, str, Latencies]:
    """Implementation, load line and (agent, event) -> (p50, p99)."""
    text = path.read_text()
    impl = re.search(r"## Benchmarks: (\w+)", text)
    load = re.search(r"Load average.*", text)
    latencies: Latencies = {}
    agent = None
    for line in text.splitlines():
        if line.startswith("Hook latency, "):
            agent = line.removeprefix("Hook latency, ").split(" ")[0]
        elif line.startswith(("Resident", "CPU")):
            agent = None
        if agent and (m := ROW.match(line)):
            latencies[(agent, m.group(1))] = (m.group(3), m.group(5))
    return (impl.group(1) if impl else path.stem), (load.group(0) if load else "?"), latencies


def main() -> None:
    (a_name, a_load, a), (b_name, b_load, b) = (read(Path(p)) for p in sys.argv[1:3])
    print(f"- {a_name}: {a_load}\n- {b_name}: {b_load}\n")
    print(f"Hook latency, ms (p50 / p99):\n\n| Agent | Event | {a_name} | {b_name} |\n|---|---|---|---|")
    for key, (p50, p99) in a.items():
        other = b.get(key)
        right = f"{other[0]} / {other[1]}" if other else "-"
        print(f"| {key[0]} | {key[1]} | {p50} / {p99} | {right} |")


if __name__ == "__main__":
    main()
