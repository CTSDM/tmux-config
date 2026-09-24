# Reproducers for tmux upstream (L2)

Two tmux 3.6 bugs and one 3.7c bug this setup ran into, reproduced on vanilla tmux (the
`tmux` first in PATH, `-f /dev/null`), on isolated servers (`-L`) killed by
name at the end. Nothing of this repo is loaded.

## tmux-L-client-name-crash.sh: server crash

A client that has connected to the server but not identified itself yet has
no name; `#{client_name}` for it inside a `#{L:}` loop is a `strdup(NULL)`
and the server dies (segfault in libc). The script holds such a connection
open (a raw socket that says nothing) and runs
`display -p '#{L:#{client_name},}'`. Guarding the name with
`#{?client_session,…}` avoids it. In real use there is nearly always such a
client while hooks run: any `tmux` command that is still connecting.

## tmux-loop-leak.sh: memory leak in session loops

Every `#{S:…}` loop a status format expands leaks copies of its body, each
time the line is drawn, for good: the server's RSS grows linearly with
redraws. `#{W:…}` and `#{P:…}` on their own don't. Redrawn through one
control client (`refresh-client -S`, paced, no new process per redraw):

| Status line | Redraws | Server RSS growth |
|---|---|---|
| 100 × `#{S:x}` | 2000 | 870 bytes per redraw |
| 100 × `#{S:x}` | 6000 | 841 bytes per redraw (no plateau) |
| 100 × `#{S:#{session_name}}` | 2000 | 1695 bytes per redraw (a bigger body) |
| 100 × `#{S:#{W:x}}` | 2000 | 868 bytes per redraw |
| 100 × `#{W:x}`, 100 × `#{P:x}` | 2000 | 22 bytes per redraw |
| 100 × `#{session_name}` | 2000 | 16 bytes per redraw |

The same with 4 or 32 sessions: the leak is per loop expansion, and grows
with what is expanded inside it. A bar that expands many session loops per
redraw grows the server by kilobytes per redraw (here ~4 KB, 6 GB after a
day).

Both bugs are fixed upstream since tmux 3.7: the leak in e6035495
(issue #4898, `format_loop_sessions` did not free its copies of the body),
the crash in 3c3d9ce3 (the client loop uses `sort_get_clients`, which skips
clients that are not attached). On a fixed tmux both scripts should come
out clean: no crash, flat.

```sh
./tmux-loop-leak.sh loop 2000
./tmux-loop-leak.sh '#{W:x}' 2000   # any format
./tmux-loop-leak.sh plain 2000
PATH=/path/to/tmux-3.7/bin:$PATH ./tmux-loop-leak.sh loop 2000
```

## tmux-popup-overlay.sh: a popup's top rows lost (3.7c)

With the status line at the top, a pane that prints under a popup paints
over the popup's first rows, one per status line: here (2 lines) its border
with the title and the blank row under it, the `prefix N` form losing its
" new session " header now and then. The damage stays until the next full
redraw. `screen_redraw_draw_pane` asks which cells the popup covers with the
pane row's position in the window (`wy`), where `tty_check_overlay_range`
wants the position on the terminal (`py`, computed just above); the two
differ by the status lines at the top. Status at the bottom: no bug.

tmux master has replaced popups with floating panes, so no 3.7 release is
expected to fix it: `tmux-3.7c-popup-overlay.patch` (one line) goes on the
3.7c release tarball before building. The tarball needs a `yacc` (bison).

```sh
./tmux-popup-overlay.sh          # 3.7c: 10/10 lost; patched: 0/10
./tmux-popup-overlay.sh 1 10     # one status line at the top
PATH=/path/to/patched/tmux/dir:$PATH ./tmux-popup-overlay.sh
```
