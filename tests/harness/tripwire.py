"""Tripwires: the last line of defense against real sounds and notifications.

The test environment points the sound player (AG_SOUND_PLAYER) at a script
that only records its call, leaves DBUS_SESSION_BUS_ADDRESS unset and puts a
socket that only records connections where D-Bus clients then look
($XDG_RUNTIME_DIR/bus). A test that trips either one fails: something tried
to play or show for real instead of writing to the sink.
"""

import socket
import struct
import threading
from pathlib import Path


def player_script(path: Path, log: Path) -> None:
    path.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >>"{log}"\n')
    path.chmod(0o755)


class Bus:
    """A Unix socket at `path` that accepts and drops every connection."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.hits = 0
        self.callers: list[str] = []
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.bind(str(path))
        self._sock.listen(8)
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self) -> None:
        while True:
            try:
                conn, _ = self._sock.accept()
            except OSError:
                return  # closed
            pid, _, _ = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            try:
                comm = Path(f"/proc/{pid}/comm").read_text().strip()
                cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode()
            except OSError:
                comm = cmdline = "?"
            # tmux itself asks systemd (user bus) for a scope per pane: not ours.
            if not comm.startswith("tmux"):
                self.hits += 1
                self.callers.append(f"{pid} {cmdline.strip()}")
            conn.close()

    def close(self) -> None:
        self._sock.shutdown(socket.SHUT_RDWR)
        self._sock.close()
        self._thread.join(timeout=1)
