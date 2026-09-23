import pytest

from .impl import IMPL


def rule(*ids: str) -> pytest.MarkDecorator:
    """The contract rules (docs/daemon/contract.md) a test covers."""
    return pytest.mark.rule(*ids)


def change(rule_id: str) -> pytest.MarkDecorator:
    """A CHANGE rule: bash keeps the old behavior, so the test must fail there."""
    return pytest.mark.xfail(
        IMPL.name == "bash",
        reason=f"{rule_id} is an intentional change; bash keeps the old behavior",
        strict=True,
    )
