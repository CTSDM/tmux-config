from typing import Any

from . import Message

class DBusErrorResponse(Exception):
    name: str
    data: Any

def unwrap_msg(msg: Message) -> tuple[Any, ...]: ...
