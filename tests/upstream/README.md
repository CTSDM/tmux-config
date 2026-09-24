# Reproducers for tmux upstream (L2)

Two tmux 3.6 bugs this setup ran into, reproduced on vanilla tmux: `-f
/dev/null`, isolated servers (`-L`), killed by name at the end. Nothing of
this repo is loaded.

## tmux-L-client-name-crash.sh: server crash

A client that has connected to the server but not identified itself yet has
no name; `#{client_name}` for it inside a `#{L:}` loop is a `strdup(NULL)`
and the server dies (segfault in libc). The script holds such a connection
open (a raw socket that says nothing) and runs
`display -p '#{L:#{client_name},}'`. Guarding the name with
`#{?client_session,…}` avoids it. In real use there is nearly always such a
client while hooks run: any `tmux` command that is still connecting.

## tmux-loop-leak.sh: memory leak on loops

Every `#{S:…}` (also `#{W:…}`, `#{P:…}`) loop a status format expands leaks
a little memory, each time the line is drawn, for good: the server's RSS
grows linearly with redraws. A status line of 100 × `#{S:x}`, redrawn through
one control client (`refresh-client -S`, paced, no new process per redraw):

| Line | Redraws | Server RSS growth |
|---|---|---|
| 100 × `#{S:x}` | 2000 | 872 bytes per redraw |
| 100 × `#{S:x}` | 6000 | 841 bytes per redraw (no plateau) |
| 100 × `#{session_name}` | 2000 | 10 bytes per redraw |

About 8.5 bytes per loop expansion, the same with 4 or 32 sessions; nested
loops leak more (`#{S:#{W:#{P:#{session_name}}}}`: ~33 bytes). A bar that
expands many loops per redraw grows the server by kilobytes per redraw (here
~4 KB, 6 GB after a day).

```sh
./tmux-loop-leak.sh loop 2000
./tmux-loop-leak.sh plain 2000
```
