#!/bin/bash
# tmux 3.6: every #{S:...} (also #{W:...}, #{P:...}) loop in a status format
# leaks a little memory each time the status line is drawn. The server's RSS
# grows linearly with redraws; the same line without loops stays flat.
#
#   ./tmux-loop-leak.sh [loop|plain] [redraws]
#
# Vanilla tmux, isolated servers (-L), no config; both killed by name at the end.
set -u
kind=${1:-loop} redraws=${2:-2000}
name=loop-leak-$$
t() { env -u TMUX -u TMUX_PANE tmux -L "$name" "$@"; }
term() { env -u TMUX -u TMUX_PANE tmux -L "$name-term" "$@"; }

if [[ $kind == loop ]]; then unit='#{S:x}'; else unit='#{session_name}'; fi
line=$(for _ in $(seq 100); do printf '%s' "$unit"; done)

t -f /dev/null new-session -d -s main -x 200 -y 50 'sleep 600'
t new-session -d -s other 'sleep 600'
t set -g 'status-format[0]' "$line"
# A real client, so that the status line is drawn: attached from a pane of a
# second server, which plays the terminal.
term -f /dev/null new-session -d -x 200 -y 50 "env -u TMUX tmux -L $name attach -t main"
for _ in $(seq 50); do client=$(t list-clients -F '#{client_name}' 2>/dev/null | head -n1); [[ -n $client ]] && break; sleep 0.1; done
pid=$(t display -p '#{pid}')
socket=$(t display -p '#{socket_path}') term_socket=$(term display -p '#{socket_path}')
rss() { awk '/^VmRSS/ { print $2 }' "/proc/$pid/status"; }

# Redraws through a single control client, paced (no new process per redraw).
redraw() {
  { echo 'refresh-client -f no-output'
    for _ in $(seq "$1"); do echo "refresh-client -S -t $client"; sleep 0.01; done
  } | t -C attach -t other >/dev/null
}
redraw 300 # warm up
before=$(rss)
redraw "$redraws"
after=$(rss)
echo "tmux $(tmux -V | cut -d' ' -f2), status line of 100 x '$unit':" \
  "RSS $before -> $after kB after $redraws redraws, $(( (after - before) * 1024 / redraws )) bytes per redraw"
term kill-server 2>/dev/null; t kill-server 2>/dev/null
rm -f "$socket" "$term_socket"
