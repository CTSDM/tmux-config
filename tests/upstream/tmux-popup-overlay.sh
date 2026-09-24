#!/bin/bash
# tmux 3.7c: with the status line at the top, a pane that prints under a
# popup paints over the popup's first rows (its border and title), one row
# per status line, and they stay broken until the next full redraw.
#
#   ./tmux-popup-overlay.sh [status lines at the top, 1-5, default 2] [tries]
#
# Vanilla tmux, isolated servers (-L), no config; both killed by name at the end.
set -u
lines=${1:-2} tries=${2:-10}
name=popup-overlay-$$
t() { env -u TMUX -u TMUX_PANE tmux -L "$name" "$@"; }
term() { env -u TMUX -u TMUX_PANE tmux -L "$name-term" "$@"; }

t -f /dev/null new-session -d -s main -x 120 -y 40 \
  "bash -c 'i=0; while :; do echo \"line \$((i++)) under the popup\"; sleep 0.02; done'"
t set -g status-position top
t set -g status "$([[ $lines == 1 ]] && echo on || echo "$lines")"
# A real client, so that the popup is drawn: attached from a pane of a
# second server, which plays the terminal.
term -f /dev/null new-session -d -x 120 -y 40 "env -u TMUX tmux -L $name attach -t main"
for _ in $(seq 50); do client=$(t list-clients -F '#{client_name}' 2>/dev/null | head -n1); [[ -n $client ]] && break; sleep 0.1; done

bad=0
for _ in $(seq "$tries"); do
  t display-popup -c "$client" -E -w 60 -h 10 -T ' title ' "printf '\nfirst row\n'; sleep 3" &
  sleep 1.5
  screen=$(term capture-pane -p)
  # The border with the title must sit two rows above the first text row.
  top=$(grep -n ' title ' <<<"$screen" | head -n1 | cut -d: -f1)
  first=$(grep -n 'first row' <<<"$screen" | head -n1 | cut -d: -f1)
  [[ -n $top && -n $first && $((first - top)) -eq 2 ]] || bad=$((bad + 1))
  wait
done
echo "$(t -V), status $lines at the top: popups with a lost top $bad/$tries"
term kill-server; t kill-server
