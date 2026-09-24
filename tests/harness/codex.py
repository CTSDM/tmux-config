"""Codex helpers: a synthetic rollout (the session's JSONL transcript) and a
driver that fills in the turn and the rollout path of each hook."""

import json
import os
from pathlib import Path
from typing import Any

from .agent import FakeAgent, HookResult

type Record = dict[str, Any]


def event(kind: str, turn: str | None, **fields: Any) -> Record:
    payload: Record = {"type": kind, **fields}
    if turn is not None:
        payload["turn_id"] = turn
    return {"type": "event_msg", "payload": payload}


def call_output(call_id: str) -> Record:
    return {"type": "response_item", "payload": {"type": "function_call_output", "call_id": call_id}}


class Rollout:
    def __init__(self, path: Path) -> None:
        self.path = path
        path.write_text("")

    def append(self, *records: Record | str) -> None:
        """Records as JSON lines; a str is written as is (e.g. half a line)."""
        with self.path.open("a") as f:
            f.write("".join(r if isinstance(r, str) else json.dumps(r) + "\n" for r in records))

    def started(self, turn: str, mode: str | None = None) -> None:
        fields = {"collaboration_mode_kind": mode} if mode else {}
        self.append(event("task_started", turn, **fields))

    def complete(self, turn: str | None, message: str | None = None, error: Record | None = None) -> None:
        fields: Record = {}
        if message is not None:
            fields["last_agent_message"] = message
        if error is not None:
            fields["error"] = error
        self.append(event("task_complete", turn, **fields))

    def aborted(self, turn: str | None) -> None:
        self.append(event("turn_aborted", turn))

    def output(self, call_id: str) -> None:
        self.append(call_output(call_id))

    def replace(self, *records: Record) -> None:
        """A new file at the same path (e.g. rotation)."""
        new = self.path.with_suffix(".new")
        new.write_text("".join(json.dumps(r) + "\n" for r in records))
        os.replace(new, self.path)


_UNSET: Any = object()


class Codex:
    """A fake codex agent and its rollout, one turn at a time."""

    def __init__(self, agent: FakeAgent, rollout: Rollout, turn: str = "one") -> None:
        self.agent = agent
        self.rollout = rollout
        self.turn = turn

    @property
    def pane(self) -> str:
        return self.agent.pane

    def hook(self, event_name: str, /, turn_id: Any = _UNSET, **fields: Any) -> HookResult:
        """An event of the current turn (turn_id=None: without a turn id)."""
        turn = self.turn if turn_id is _UNSET else turn_id
        if turn is not None:
            fields["turn_id"] = turn
        fields.setdefault("transcript_path", str(self.rollout.path))
        return self.agent.hook(event_name, **fields)

    def start(self) -> None:
        """SessionStart and a first prompt, with its task_started."""
        self.hook("SessionStart", source="startup")
        self.prompt(self.turn)

    def prompt(self, turn: str, mode: str | None = None) -> None:
        self.turn = turn
        self.hook("UserPromptSubmit")
        self.rollout.started(turn, mode)

    def pre(self, tool: str, call: str, **tool_input: Any) -> None:
        self.hook("PreToolUse", tool_name=tool, tool_use_id=call, tool_input=tool_input)

    def ask(self, tool: str, **tool_input: Any) -> None:
        """A PermissionRequest: Codex sends no tool id with it."""
        self.hook("PermissionRequest", tool_name=tool, tool_input=tool_input)

    def post(self, tool: str, call: str, **fields: Any) -> None:
        self.hook("PostToolUse", tool_name=tool, tool_use_id=call, **fields)
