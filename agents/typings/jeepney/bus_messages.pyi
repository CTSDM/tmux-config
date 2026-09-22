from . import Message

class MatchRule:
    def __init__(
        self,
        *,
        type: str | None = ...,
        sender: str | None = ...,
        interface: str | None = ...,
        member: str | None = ...,
        path: str | None = ...,
        path_namespace: str | None = ...,
        destination: str | None = ...,
        eavesdrop: bool = ...,
    ) -> None: ...

class DBus:
    def AddMatch(self, rule: MatchRule | str) -> Message: ...

message_bus: DBus
