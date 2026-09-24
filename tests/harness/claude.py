"""Helpers shared by the contract tests."""

import time

from .agent import FakeAgent


# The options of the contract's table (§2). Others are internal.
CONTRACT_OPTIONS = frozenset(
    {
        "@agent", "@agent_state", "@agent_needs", "@agent_needs_id", "@agent_since",
        "@agent_prev", "@agent_tool", "@agent_msg", "@agent_subs", "@agent_subtypes",
        "@agent_bg", "@agent_session", "@agent_transcript", "@agent_mode", "@agent_model",
        "@agent_profile", "@agent_tests_sound_at", "@agent_turn", "@agent_outcome",
        "@agent_question", "@agent_collaboration", "@agent_pid", "@agent_pid_start",
    }
)  # fmt: skip


def shown(agent: FakeAgent) -> dict[str, str]:
    """The contract's options as their readers see them: a format cannot tell
    an unset option from an empty one, and `@agent_subs` 0 means none (§2).
    Internal options (notification ids, watcher pids) are left out."""
    return {
        k: v
        for k, v in agent.options().items()
        if k in CONTRACT_OPTIONS and v != "" and not (k == "@agent_subs" and v == "0")
    }


def state(agent: FakeAgent) -> str:
    return agent.option("@agent_state")


def working(agent: FakeAgent, **fields: str) -> None:
    """A fresh session that is working on a prompt."""
    agent.hook("SessionStart", source="startup", **fields)
    agent.hook("UserPromptSubmit")
    assert state(agent) == "working"


def pre(agent: FakeAgent, tool: str, call: str, **tool_input: str) -> None:
    agent.hook("PreToolUse", tool_name=tool, tool_use_id=call, tool_input=tool_input)


def ask(agent: FakeAgent, tool: str, call: str, **tool_input: str) -> None:
    agent.hook("PermissionRequest", tool_name=tool, tool_use_id=call, tool_input=tool_input)


def post(agent: FakeAgent, tool: str, call: str, event: str = "PostToolUse") -> None:
    agent.hook(event, tool_name=tool, tool_use_id=call)


def next_second() -> None:
    """Sleep past the next whole second, so a new `@agent_since` differs."""
    time.sleep(1.05 - time.time() % 1)


def near_now(epoch: str, slack: int = 3) -> bool:
    return epoch.isdigit() and abs(int(epoch) - time.time()) <= slack
