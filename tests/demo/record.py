"""The README's demo: a scripted session on an isolated tmux server with this
checkout's config and the contract suite's fake agents (nothing of yours shows,
nothing reaches your desktop), recorded by asciinema 3. render.py turns it
into the GIF.

    cd tests && uv run python demo/record.py /tmp/demo.cast
"""

import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tests"))

from harness.agent import FakeAgent, make_fakes
from harness.tmux import TmuxServer

COLS, ROWS = 110, 20
OUT = Path(sys.argv[1]).resolve()
CAPS = OUT.with_suffix(".captions.json")


def demo_conf(root: Path) -> Path:
    """tmux.conf as the user loads it, minus TPM (its plugins live in the
    real home), with truecolor for the recorder's terminal."""
    text = (REPO / "tmux.conf").read_text().replace("#{d:current_file}", str(REPO))
    lines = [l for l in text.splitlines() if "plugins/tpm/tpm" not in l]
    lines.append("set -as terminal-features ',xterm-256color:RGB'")
    lines.append("set -g status-interval 1")
    conf = root / "demo.conf"
    conf.write_text("\n".join(lines) + "\n")
    return conf


def main() -> None:
    base = Path(tempfile.mkdtemp(prefix="agdemo-", dir="/tmp"))
    fakes = make_fakes(base / "fakes")
    root = base / "srv"
    server = TmuxServer(root, fakes, conf=False, theme=False, agentd=True)
    home = root / "home"
    try:
        run(server, home, root)
    finally:
        server.kill()
        shutil.rmtree(base, ignore_errors=True)


def run(server: TmuxServer, home: Path, root: Path) -> None:
    t = server.tmux
    (root / "spaces.conf").write_text(f"work {home}/code/acme\npersonal *\n")
    dirs = {
        "api": home / "code/acme/api",
        "web": home / "code/acme/web",
        "infra": home / "code/acme/infra",
        "dotfiles": home / "dotfiles",
    }
    for d in dirs.values():
        d.mkdir(parents=True, exist_ok=True)
    server._setenv("AG_FOCUS_CLIENT", None)
    t("source-file", str(demo_conf(root)))

    agents: dict[str, FakeAgent] = {}

    def session(name: str) -> None:
        t(
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            str(dirs[name]),
            "-x",
            str(COLS),
            "-y",
            str(ROWS),
        )

    def agent(key: str, sess: str, title: str, screen: list[str]) -> FakeAgent:
        a = server.agent("claude", sess, cwd=str(dirs[sess]))
        t("select-pane", "-t", a.pane, "-T", f"✳ {title}")
        say(a, screen)
        a.hook("SessionStart", source="startup")
        agents[key] = a
        return a

    def say(a: FakeAgent, lines: list[str]) -> None:
        tty = t("display", "-p", "-t", a.pane, "#{pane_tty}")
        with open(tty, "w") as f:
            f.write("\x1b[?25l" + "\n".join(lines) + "\n")

    for name in ("api", "web", "infra", "dotfiles"):
        session(name)
    t("kill-session", "-t", "main")

    api1 = agent(
        "api1",
        "api",
        "Fix login redirect",
        [
            "\x1b[1m> Fix the redirect loop after the OAuth callback\x1b[0m",
            "",
            "  Reading src/auth/callback.ts",
            "  Reading src/auth/session.ts",
            "  Editing src/auth/session.ts  +12 -4",
        ],
    )
    api2 = agent(
        "api2",
        "api",
        "Add rate limiting",
        [
            "\x1b[1m> Add rate limiting to the public endpoints\x1b[0m",
            "",
            "  Searching for 'router.use'",
            "  Reading src/http/middleware.ts",
        ],
    )
    web1 = agent(
        "web1",
        "web",
        "Migrate to Vite",
        [
            "\x1b[1m> Migrate the build from webpack to Vite\x1b[0m",
            "",
            "  Editing vite.config.ts",
            "  Removed webpack.config.js",
            "  Done: the dev server starts in 300 ms.",
        ],
    )
    web2 = agent(
        "web2",
        "web",
        "Deploy preview",
        [
            "\x1b[1m> Deploy a preview of this branch\x1b[0m",
            "",
            "  Running make preview",
        ],
    )
    infra = agent(
        "infra",
        "infra",
        "Audit dependencies",
        [
            "\x1b[1m> Audit the dependencies and open issues\x1b[0m",
            "",
            "  Running cargo audit",
            "  Spawned 2 subagents",
        ],
    )
    dots = agent(
        "dots",
        "dotfiles",
        "Tidy zshrc",
        [
            "\x1b[1m> Remove plugins I no longer use\x1b[0m",
            "",
            "  Done.",
        ],
    )
    for s in dirs:
        t("kill-window", "-t", f"{s}:1")
        t("select-window", "-t", f"{s}:1")

    # Starting states.
    for a in (api1, api2, web2, infra):
        a.hook("UserPromptSubmit")
    for a in (web1, dots):
        a.hook("UserPromptSubmit")
        a.hook("Stop", last_assistant_message="Done.")
    t("run-shell", "-b", f"{server.impl.bin}/agent-spaces assign")
    time.sleep(1.5)

    # The recorder: a real client, attached to api.
    env = {
        **server.env,
        "TERM": "xterm-256color",
        "COLUMNS": str(COLS),
        "LINES": str(ROWS),
    }
    rec = subprocess.Popen(
        [
            "asciinema",
            "rec",
            "--overwrite",
            "--headless",
            "--window-size",
            f"{COLS}x{ROWS}",
            "--command",
            f"tmux -L {server.name} attach -t api",
            str(OUT),
        ],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    client = ""
    for _ in range(100):
        client = t(
            "list-clients",
            "-F",
            "#{client_name}",
            "-f",
            "#{==:#{client_control_mode},0}",
        )
        if client:
            break
        time.sleep(0.1)
    assert client, "the recorder never attached"
    t0 = time.monotonic() - 0.3

    def keys(*k: str, pause: float = 0.6) -> None:
        for key in k:
            t("send-keys", "-K", "-c", client, key)
            time.sleep(0.15)
        time.sleep(pause)

    caps: list[list] = []

    def caption(text: str, secs: float) -> None:
        """Shown under the video for `secs` from now (added by ffmpeg)."""
        now = time.monotonic() - t0
        caps.append([round(now, 2), round(now + secs, 2), text])
        time.sleep(secs)

    time.sleep(1.0)
    caption(
        "Every Claude Code and Codex pane reports its state: ● working  ✓ done  ○ idle",
        3.5,
    )

    # Agents run in the background.
    infra.hook("SubagentStart", agent_id="a1", agent_type="Explore")
    infra.hook("SubagentStart", agent_id="a2", agent_type="Plan")
    infra.hook("Stop", last_assistant_message="Waiting on two subagents.")
    shell = api2.spawn(
        [
            "/bin/bash",
            "-c",
            "source /x/shell-snapshots/snapshot-bash-1.sh 2>/dev/null; sleep 300; exit 0",
        ]
    )
    api2.hook("Stop", last_assistant_message="The load test runs in the background.")
    time.sleep(1.0)
    caption("Turn over, work still running: ◐ subagents, ○゙ shells", 3.5)

    # One needs you.
    web2.hook(
        "PreToolUse",
        tool_name="Bash",
        tool_use_id="t1",
        tool_input={"command": "make deploy-preview"},
    )
    web2.hook(
        "PermissionRequest",
        tool_name="Bash",
        tool_use_id="t1",
        tool_input={"command": "make deploy-preview"},
    )
    caption(
        "▲ An agent needs you: its session and tab blink, a notification pops up", 4.0
    )
    now = time.monotonic() - t0
    caps.append([round(now, 2), round(now + 2.8, 2), "prefix u: straight to it"])
    keys("M-a", "u", pause=2.6)
    say(web2, ["", "  Allow: make deploy-preview?  \x1b[1m1. Yes\x1b[0m  2. No"])
    time.sleep(1.0)
    web2.hook("PostToolUse", tool_name="Bash", tool_use_id="t1")
    say(web2, ["  Preview: https://preview.example.com"])
    time.sleep(1.5)

    # Finished elsewhere: green until you look.
    api1.hook("Stop", last_assistant_message="Fixed the redirect loop.")
    caption("✓ Finished and not seen yet: green until you look at it", 3.5)

    # Mission control.
    now = time.monotonic() - t0
    caps.append(
        [
            round(now, 2),
            round(now + 5.5, 2),
            "prefix a: mission control, every agent in every session",
        ]
    )
    keys("M-a", "a", pause=2.2)
    keys("Down", pause=0.7)
    keys("Down", pause=1.2)
    keys("Enter", pause=1.5)

    # Sessions by number, spaces.
    now = time.monotonic() - t0
    caps.append(
        [
            round(now, 2),
            round(now + 4.5, 2),
            "prefix g 3: a session by number   prefix ( ) step through them",
        ]
    )
    keys("M-a", "g", "3", pause=2.0)
    keys("M-a", ")", pause=2.0)
    keys("M-a", "g", "1", pause=1.0)
    api2.signal(shell)
    time.sleep(3.0)

    t("detach-client", "-t", client)
    rec.wait(timeout=10)
    CAPS.write_text(json.dumps(caps, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()
