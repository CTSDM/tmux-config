#!/usr/bin/env python3
"""Run: python3 -B agents/tests/test_codex.py (isolated tmux, no API calls)."""

import ctypes
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BIN = Path(__file__).resolve().parents[1] / "bin"
loader = importlib.machinery.SourceFileLoader("agent_codex", str(BIN / "agent-codex"))
spec = importlib.util.spec_from_loader(loader.name, loader)
codex = importlib.util.module_from_spec(spec)
loader.exec_module(codex)


def lifecycle(kind, turn="one", **fields):
    return (
        json.dumps(
            {"type": "event_msg", "payload": {"type": kind, "turn_id": turn, **fields}}
        )
        + "\n"
    )


class RolloutTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "rollout.jsonl"

    def test_error_and_more_than_300_later_records(self):
        self.path.write_text(
            lifecycle("task_started")
            + lifecycle("task_complete", error={"message": "limit"})
            + "null\n" * 350
        )
        result = codex.read_rollout(str(self.path), {})
        self.assertEqual(result["status"], "error")
        self.assertEqual(result["error"]["message"], "limit")

    def test_old_abort_cannot_finish_new_turn(self):
        self.path.write_text(
            lifecycle("task_started", "new") + lifecycle("turn_aborted", "old")
        )
        self.assertEqual(codex.read_rollout(str(self.path), {})["status"], "busy")

    def test_partial_append_and_rotation(self):
        end = lifecycle("task_complete")
        self.path.write_text(lifecycle("task_started") + end[:15])
        cache = codex.read_rollout(str(self.path), {})
        self.assertEqual(cache["status"], "busy")
        with self.path.open("a") as f:
            f.write(end[15:])
        cache = codex.read_rollout(str(self.path), cache)
        self.assertEqual(cache["status"], "complete")
        replacement = self.path.with_suffix(".new")
        replacement.write_text(lifecycle("task_started", "two"))
        replacement.replace(self.path)
        cache = codex.read_rollout(str(self.path), cache)
        self.assertEqual((cache["turn"], cache["status"]), ("two", "busy"))

    def test_missing_transcript_is_not_completion(self):
        self.assertEqual(codex.read_rollout(str(self.path), {}), {})


class TransitionTests(unittest.TestCase):
    def setUp(self):
        self.data = {}
        self.current = {"agent_state": "working"}
        self.event("SessionStart", source="startup")
        self.event("UserPromptSubmit")

    def event(self, kind, **fields):
        return codex.transition(
            self.data,
            {
                "hook_event_name": kind,
                "session_id": "session",
                "turn_id": "one",
                **fields,
            },
            self.current,
            {},
        )

    def test_parallel_permission_without_id(self):
        self.event(
            "PreToolUse",
            tool_name="Bash",
            tool_use_id="a",
            tool_input={"command": "echo A"},
        )
        self.event(
            "PreToolUse",
            tool_name="Bash",
            tool_use_id="b",
            tool_input={"command": "echo B"},
        )
        self.event(
            "PermissionRequest",
            tool_name="Bash",
            tool_input={"command": "echo A", "description": "approval"},
        )
        result = self.event("PostToolUse", tool_name="Bash", tool_use_id="b")
        self.assertEqual(result[:2], ("needs", "permission"))
        self.assertEqual(list(self.data["pending"]), ["a"])
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="a")[0], "working"
        )

    def test_unmatched_permission_is_conservative(self):
        self.assertEqual(
            self.event(
                "PermissionRequest",
                tool_name="Bash",
                tool_input={"command": "missing pre"},
            )[0],
            "needs",
        )
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="other")[0], "needs"
        )
        self.assertEqual(self.event("Interrupt")[0], "idle")

    def test_identical_parallel_commands_stay_pending_until_both_return(self):
        for call in ("a", "b"):
            self.event(
                "PreToolUse",
                tool_name="Bash",
                tool_use_id=call,
                tool_input={"command": "echo same"},
            )
        self.event(
            "PermissionRequest", tool_name="Bash", tool_input={"command": "echo same"}
        )
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="a")[0], "needs"
        )
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="b")[0], "working"
        )

    def test_question_and_other_tool_completion(self):
        self.event("PreToolUse", tool_name="request_user_input", tool_use_id="q")
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="other")[:2],
            ("needs", "question"),
        )
        self.assertEqual(
            self.event("PostToolUse", tool_name="request_user_input", tool_use_id="q")[
                0
            ],
            "working",
        )

    def test_late_result_and_other_session_are_ignored(self):
        self.event("Stop")
        self.assertEqual(
            self.event("PostToolUse", tool_name="Bash", tool_use_id="old")[0], ""
        )
        self.assertIsNone(self.event("PostToolUse", turn_id="old"))
        self.assertIsNone(self.event("SessionEnd", session_id="old"))

    def test_async_ack_is_not_a_user_answer(self):
        self.event(
            "PostToolUse",
            tool_name="request_user_input_async",
            tool_response='{"accepted":true}',
        )
        self.assertTrue(self.data["question"])
        self.event("Stop")
        self.assertTrue(self.data["question"])
        self.event("UserPromptSubmit", turn_id="two")
        self.assertFalse(self.data["question"])

    def test_subagent_turn_does_not_replace_parent(self):
        self.assertIsNotNone(
            self.event("SubagentStart", agent_id="child", turn_id="child-turn")
        )
        self.assertEqual(self.data["turn"], "one")


def worker(root):
    """Fake only the agent identity; run the actual hook, observer and processes."""
    ctypes.CDLL(None).prctl(15, b"codex", 0, 0, 0)
    pane = os.environ["TMUX_PANE"]
    path = root / "rollout.jsonl"
    path.write_text("")
    turn = "one"
    checks = []
    children = []
    process_groups = []

    def hook(kind, agent_kind="codex", **fields):
        payload = {
            "hook_event_name": kind,
            "session_id": "test-session",
            "turn_id": turn,
            "transcript_path": str(path),
            "permission_mode": "bypassPermissions",
            **fields,
        }
        subprocess.run(
            [str(BIN / "agent-hook"), agent_kind],
            input=json.dumps(payload),
            text=True,
            check=True,
        )

    def option(name):
        return subprocess.check_output(
            ["tmux", "show", "-pqv", "-t", pane, name], text=True
        ).strip()

    def expect(name, expected, timeout=0):
        end = time.monotonic() + timeout
        while True:
            value = option(name)
            if value == expected:
                checks.append([name, expected])
                return
            if time.monotonic() >= end:
                raise AssertionError(f"{name}: expected {expected!r}, got {value!r}")
            time.sleep(0.1)

    def append(text):
        with path.open("a") as stream:
            stream.write(text)

    try:
        hook("SessionStart", source="startup")
        hook("UserPromptSubmit")
        append(lifecycle("task_started", collaboration_mode_kind="plan"))
        hook("PreToolUse", tool_name="request_user_input", tool_use_id="q")
        expect("@agent_state", "needs")
        expect("@agent_needs", "question")
        hook(
            "PreToolUse",
            tool_name="Bash",
            tool_use_id="other",
            tool_input={"command": "true"},
        )
        hook("PostToolUse", tool_name="Bash", tool_use_id="other")
        expect("@agent_needs", "question")
        hook("PostToolUse", tool_name="request_user_input", tool_use_id="q")
        expect("@agent_state", "working")
        expect("@agent_collaboration", "plan")
        hook(
            "PostToolUse",
            tool_name="request_user_input_async",
            tool_response='{"accepted":true}',
        )
        expect("@agent_question", "sent")
        labels = root / "labels.conf"
        labels.write_text(
            "\n".join(
                line
                for line in (BIN.parent / "agents.conf").read_text().splitlines()
                if line.startswith(("set -g @agent-label ", "set -ag @agent-label "))
            )
        )
        subprocess.run(["tmux", "source-file", str(labels)], check=True)
        label = subprocess.check_output(
            ["tmux", "display", "-p", "-t", pane, "#{E:@agent-label}"], text=True
        ).strip()
        assert label == "working · plan mode · question sent", label
        hook(
            "SubagentStart", agent_id="child", agent_type="worker", turn_id="child-turn"
        )
        expect("@agent_subs", "1")
        hook(
            "SubagentStop", agent_id="child", agent_type="worker", turn_id="child-turn"
        )
        expect("@agent_subs", "0")
        hook("Stop", last_assistant_message="done")
        expect("@agent_state", "done")
        hook("PostToolUse", tool_name="Bash", tool_use_id="late")
        expect("@agent_state", "done")
        hook("SessionEnd", session_id="old-session")
        expect("@agent_session", "test-session")
        turn = "two"
        hook("UserPromptSubmit")
        expect("@agent_question", "")
        append(lifecycle("task_started", turn))
        hook(
            "PermissionRequest",
            tool_name="Bash",
            tool_input={"command": "test approval"},
        )
        expect("@agent_state", "needs")
        hook("Interrupt")
        expect("@agent_state", "idle")
        expect("@agent_needs", "")
        append(lifecycle("turn_aborted", turn))
        turn = "three"
        hook("UserPromptSubmit")
        append(
            lifecycle("task_started", turn)
            + lifecycle(
                "task_complete",
                turn,
                error={
                    "message": "Synthetic usage limit",
                    "codex_error_info": "usage_limit_exceeded",
                },
            )
        )
        expect("@agent_state", "error", timeout=6)
        expect("@agent_msg", "Synthetic usage limit")
        turn = "four"
        hook("UserPromptSubmit")
        append(lifecycle("task_started", turn))
        # Both inherit CODEX_THREAD_ID; MCP retains the parent's Unix session.
        env = {**os.environ, "CODEX_THREAD_ID": "test-session"}
        mcp = subprocess.Popen(["sleep", "30"], env=env)
        children.append(mcp)
        command = subprocess.Popen(
            ["/bin/sh", "-c", "sleep 30 & wait"], env=env, start_new_session=True
        )
        children.append(command)
        process_groups.append(command.pid)
        hook("Stop", last_assistant_message="background")
        append(lifecycle("task_complete", turn))
        expect("@agent_bg", "1")
        expect("@agent_state", "done")
        os.killpg(command.pid, signal.SIGTERM)
        command.wait(timeout=3)
        expect("@agent_bg", "", timeout=6)
        assert mcp.poll() is None
        expect("@agent_codex_watch", "", timeout=6)
        turn = "five"
        hook("UserPromptSubmit")
        append(lifecycle("task_started", turn))
        detached = subprocess.Popen(
            ["/bin/sh", "-c", "sleep 30 & sleep 2"], env=env, start_new_session=True
        )
        children.append(detached)
        process_groups.append(detached.pid)
        hook("Stop", last_assistant_message="daemon")
        append(lifecycle("task_complete", turn))
        expect("@agent_bg", "1")
        detached.wait(timeout=4)
        time.sleep(2.2)  # Let the observer scan after the shell has exited.
        expect("@agent_bg", "1")
        os.killpg(detached.pid, signal.SIGTERM)
        expect("@agent_bg", "", timeout=6)
        hook("SessionEnd")
        expect("@agent", "")
        # Shared publishing must still work for Claude after the Codex changes.
        ctypes.CDLL(None).prctl(15, b"claude", 0, 0, 0)
        hook("SessionStart", agent_kind="claude", source="startup")
        hook("UserPromptSubmit", agent_kind="claude")
        hook(
            "PermissionRequest",
            agent_kind="claude",
            tool_name="AskUserQuestion",
            tool_use_id="claude-q",
        )
        expect("@agent_needs", "question")
        hook(
            "PostToolUse",
            agent_kind="claude",
            tool_name="AskUserQuestion",
            tool_use_id="claude-q",
        )
        expect("@agent_state", "working")
        hook("Stop", agent_kind="claude", last_assistant_message="done")
        expect("@agent_state", "done")
        hook("SessionEnd", agent_kind="claude")
        expect("@agent", "")
        (root / "result.json").write_text(json.dumps({"checks": checks}))
    except BaseException as error:
        (root / "result.json").write_text(
            json.dumps({"error": repr(error), "checks": checks})
        )
        raise
    finally:
        for group in process_groups:
            try:
                os.killpg(group, signal.SIGTERM)
            except ProcessLookupError:
                pass
        for p in children:
            if p.poll() is None:
                p.terminate()
            p.wait(timeout=3)


class TmuxIntegrationTests(unittest.TestCase):
    def test_real_hook_observer_and_background_processes(self):
        with tempfile.TemporaryDirectory(prefix="tmux-codex-test-") as tmp:
            root = Path(tmp)
            server = "codex-test-" + str(os.getpid())
            env = {
                **os.environ,
                "XDG_RUNTIME_DIR": str(root / "runtime"),
                "XDG_STATE_HOME": str(root / "state"),
                "PYTHONDONTWRITEBYTECODE": "1",
            }
            (root / "runtime").mkdir()
            (root / "state/tmux-agents").mkdir(parents=True)
            (root / "state/tmux-agents/debug").touch()
            base = ["tmux", "-L", server]
            subprocess.run(
                [
                    *base,
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "test",
                    "sleep 30",
                ],
                env=env,
                check=True,
            )
            try:
                for args in [
                    ["set", "-g", "@agent_mute_test", "on"],
                    ["set", "-g", "@agent_sound", "off"],
                    ["set", "-g", "@agent_remind_after", "0"],
                    ["set", "-t", "test", "@space", "test"],
                    ["set-window-option", "remain-on-exit", "on"],
                ]:
                    subprocess.run([*base, *args], check=True)
                command = shlex.join(
                    [
                        sys.executable,
                        "-B",
                        str(Path(__file__).resolve()),
                        "--worker",
                        str(root),
                    ]
                )
                command += " >" + shlex.quote(str(root / "worker.log")) + " 2>&1"
                subprocess.run(
                    [*base, "respawn-pane", "-k", "-t", "test:0.0", command], check=True
                )
                end = time.monotonic() + 35
                while not (root / "result.json").exists() and time.monotonic() < end:
                    time.sleep(0.1)
                self.assertTrue(
                    (root / "result.json").exists(), (root / "worker.log").read_text()
                )
                result = json.loads((root / "result.json").read_text())
                self.assertNotIn(
                    "error",
                    result,
                    json.dumps(result)
                    + "\n"
                    + (root / "worker.log").read_text()
                    + "\n"
                    + (root / "state/tmux-agents/errors.log").read_text(),
                )
                self.assertGreaterEqual(len(result["checks"]), 20)
            finally:
                subprocess.run(
                    [*base, "kill-server"], check=False, stderr=subprocess.DEVNULL
                )


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        worker(Path(sys.argv[2]))
    else:
        unittest.main()
