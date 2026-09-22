# Minimal stubs for the parts of jeepney that agents/bin/agent-notify uses:
# jeepney ships no type information, and pyright runs in strict mode.
from enum import IntEnum
from typing import Any

from .bus_messages import MatchRule as MatchRule
from .bus_messages import message_bus as message_bus

class HeaderFields(IntEnum):
    path = 1
    interface = 2
    member = 3
    error_name = 4
    reply_serial = 5
    destination = 6
    sender = 7
    signature = 8
    unix_fds = 9

class Header:
    fields: dict[HeaderFields, Any]

class Message:
    header: Header
    body: tuple[Any, ...]

class DBusAddress:
    object_path: str
    bus_name: str | None
    interface: str | None
    def __init__(self, object_path: str, bus_name: str | None = ..., interface: str | None = ...) -> None: ...

def new_method_call(
    remote_obj: DBusAddress, method: str, signature: str | None = ..., body: tuple[Any, ...] = ...
) -> Message: ...
