#!/bin/bash
# tmux 3.6: #{client_name} of a client that has connected but not identified
# yet, inside a #{L:} loop, crashes the server (strdup of a NULL name).
# Vanilla tmux, an isolated server (-L), no config; killed by name at the end.
set -u
name=crash-repro-$$
t() { env -u TMUX -u TMUX_PANE tmux -L "$name" "$@"; }
t -f /dev/null new-session -d -s main 'sleep 600'
socket=$(t display -p '#{socket_path}')
echo "tmux $(tmux -V | cut -d' ' -f2), server pid $(t display -p '#{pid}')"
# A client that connects and says nothing (no MSG_IDENTIFY_*), held 3 s.
python3 -c 'import socket, sys, time; s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(3)' "$socket" &
holder=$!
sleep 0.5
echo "safe:  $(t display -p '#{L:#{?client_session,#{client_name},},}' 2>&1)"
echo "crash: $(t display -p '#{L:#{client_name},}' 2>&1)"
t has-session 2>/dev/null && echo "server alive" || echo "server gone"
kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null
t kill-server 2>/dev/null; rm -f "$socket"
