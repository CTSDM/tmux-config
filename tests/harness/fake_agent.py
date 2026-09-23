"""A fake coding agent, driven by the test process.

Run by a copy of the Python interpreter named `claude` or `codex`, so that
/proc/<pid>/comm matches, inside a pane of the test server. It never talks to
a model: the test sends it requests over a Unix socket (one JSON line in, one
JSON line out per connection) and it runs hooks and other commands as its own
children, which is what the ownership check (contract I4) walks.

    <copy of python> -I -B fake_agent.py --control <socket>

Stdlib only: `-I` keeps the environment's Python settings out.
"""

import json
import os
import signal
import socket
import subprocess
import sys
import time
from typing import Any

type Json = dict[str, Any]

children: dict[int, subprocess.Popen[bytes]] = {}


def environment(overrides: dict[str, str | None]) -> dict[str, str]:
    env = dict(os.environ)
    for key, value in overrides.items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = value
    return env


def wrapped(argv: list[str], depth: int) -> list[str]:
    """Put `depth` shells between this process and argv. `; exit $?` keeps
    the shell from exec'ing the command in its own place."""
    for _ in range(depth):
        argv = ["/bin/sh", "-c", '"$@"; exit $?', "sh", *argv]
    return argv


def hook(req: Json) -> Json:
    argv = wrapped(list(req["argv"]), int(req.get("wrap", 0)))
    start = time.perf_counter_ns()
    try:
        done = subprocess.run(
            argv,
            input=str(req.get("stdin", "")).encode(),
            capture_output=True,
            env=environment(req.get("env", {})),
            timeout=float(req.get("timeout", 10)),
        )
    except subprocess.TimeoutExpired as expired:
        return {
            "rc": None,
            "timeout": True,
            "stdout": (expired.stdout or b"").decode(errors="replace"),
            "stderr": (expired.stderr or b"").decode(errors="replace"),
            "ns": time.perf_counter_ns() - start,
        }
    return {
        "rc": done.returncode,
        "timeout": False,
        "stdout": done.stdout.decode(errors="replace"),
        "stderr": done.stderr.decode(errors="replace"),
        "ns": time.perf_counter_ns() - start,
    }


def spawn(req: Json) -> Json:
    process = subprocess.Popen(
        list(req["argv"]),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=environment(req.get("env", {})),
        cwd=req.get("cwd"),
        start_new_session=bool(req.get("new_session", False)),
    )
    children[process.pid] = process
    return {"pid": process.pid}


def send_signal(req: Json) -> Json:
    pid, sig = int(req["pid"]), int(req.get("sig", signal.SIGTERM))
    try:
        if req.get("group"):
            os.killpg(pid, sig)
        else:
            os.kill(pid, sig)
    except ProcessLookupError:
        return {"sent": False}
    return {"sent": True}


def wait(req: Json) -> Json:
    process = children.get(int(req["pid"]))
    if process is None:
        return {"error": "not a child"}
    try:
        return {"rc": process.wait(timeout=float(req.get("timeout", 5)))}
    except subprocess.TimeoutExpired:
        return {"rc": None}


def handle(req: Json) -> Json:
    match req.get("op"):
        case "ping":
            with open(f"/proc/{os.getpid()}/comm") as f:
                comm = f.read().strip()
            return {"pid": os.getpid(), "ppid": os.getppid(), "comm": comm}
        case "hook":
            return hook(req)
        case "spawn":
            return spawn(req)
        case "signal":
            return send_signal(req)
        case "wait":
            return wait(req)
        case "children":
            return {"pids": [pid for pid, p in children.items() if p.poll() is None]}
        case op:
            return {"error": f"unknown op {op!r}"}


def main() -> None:
    path = sys.argv[sys.argv.index("--control") + 1]
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # Bind under a temporary name, then rename: the path appears ready.
    server.bind(path + ".tmp")
    os.rename(path + ".tmp", path)
    server.listen(8)
    server.settimeout(0.1)
    while True:
        # Reap finished children, as a real agent does.
        for pid, process in list(children.items()):
            if process.poll() is not None:
                del children[pid]
        try:
            conn, _ = server.accept()
        except TimeoutError:
            continue
        with conn:
            conn.settimeout(None)
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
            if not data:
                continue
            req: Json = json.loads(data)
            if req.get("op") == "exit":
                conn.sendall(b'{"ok":true}\n')
                conn.close()
                server.close()
                os.unlink(path)
                os._exit(int(req.get("code", 0)))
            try:
                reply = handle(req)
            except Exception as error:  # report it to the test, keep serving
                reply = {"error": repr(error)}
            conn.sendall(json.dumps(reply).encode() + b"\n")


if __name__ == "__main__":
    main()
