#!/usr/bin/env bash
# agentd/remote-lab.sh: a tmux server to try remote panes (design.md, "Remote
# panes") with this checkout's agentd, beside the live one. The same config
# on `tmux -L remote-lab`, whose daemon is this checkout's release build: the
# installed agentd (the live server's) doesn't know remote panes (I6).
#
#   agentd/remote-lab.sh                   start it if needed, and attach (a new
#                                          kitty window when run inside tmux)
#   agentd/remote-lab.sh new NAME [HOST]   a session NAME whose pane is the
#                                          shell NAME held on HOST (default -,
#                                          this machine; else over ssh, with
#                                          AGENTD_REMOTE_AGENTD and AGENTD_SSH)
#   agentd/remote-lab.sh holds             the shells held on this machine
#   agentd/remote-lab.sh stop              kill the lab's tmux server (held
#                                          shells stay: exit them to end them)
#
# A real Claude in a held shell runs the hook its settings name
# ($HOME/.local/bin/agentd hook claude): only a binary with remote panes
# passes its events on; the installed one drops them. A fake agent (the
# contract suite's) or this build installed there does.
set -euo pipefail

here=$(cd "${BASH_SOURCE[0]%/*}" && pwd)
bin=$here/target/release/agentd
lab=${REMOTE_LAB:-remote-lab} # another name: a second lab beside it
# Nor what an agent that runs this passes on (a Claude Code session started
# in the lab would take itself for that agent's child).
unset_agent=()
for var in $(compgen -e | grep -E '^(CLAUDE|ANTHROPIC)'); do
    unset_agent+=(-u "$var")
done
t() { env -u TMUX -u TMUX_PANE "${unset_agent[@]}" tmux -L "$lab" "$@"; }

build() {
    cargo build --release --locked --quiet --manifest-path "$here/Cargo.toml"
}

# This server's daemon must be this build: tmux.conf started the installed
# one on load (in the background, so maybe not yet).
own_daemon() {
    local pid exe sock
    sock=$(t display -p '#{socket_path}')
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pid=$(TMUX="$sock,0,0" "$bin" ctl status 2>/dev/null |
            sed -n 's/^  "pid": \([0-9]*\),$/\1/p' || true)
        exe=$([[ -n $pid ]] && readlink "/proc/$pid/exe" || true)
        if [[ $exe == "$bin" ]]; then
            return 0
        fi
        [[ -n $pid ]] && t run-shell "$bin ctl stop" || true
        t run-shell "$bin ensure" || true
        sleep 0.3
    done
    echo "remote-lab: the daemon is not this build's ($exe)" >&2
    return 1
}

start() {
    build
    if ! t has-session 2>/dev/null; then
        t -f "$here/../tmux.conf" new-session -d -s lab -c "$HOME"
    elif [[ $(t show -gqv @agents_bin) != "$here/../agents/bin" ]]; then
        # Another config was loaded since (the live one's prefix R, before
        # it reloaded its own file): this checkout's scripts again.
        t source-file "$here/../tmux.conf"
    fi
    # For ctl seen, reconcile and the blink, until a reload sets it back.
    t set -g @agentd "$bin"
    # For the sessions prefix N makes.
    for var in AGENTD_REMOTE_AGENTD AGENTD_SSH; do
        [[ -n ${!var:-} ]] && t set-environment -g "$var" "${!var}"
    done
    own_daemon
}

case "${1:-attach}" in
attach)
    start
    if [[ -n ${TMUX:-} ]] && command -v kitty >/dev/null; then
        kitty --detach env -u TMUX -u TMUX_PANE tmux -L "$lab" attach -t lab
    else
        t attach -t lab
    fi
    ;;
new)
    name=${2:?usage: remote-lab.sh new NAME [HOST]}
    start
    pass=()
    for var in AGENTD_REMOTE_AGENTD AGENTD_SSH; do
        [[ -n ${!var:-} ]] && pass+=(-e "$var=${!var}")
    done
    t new-session -d -s "$name" -c "$HOME" "${pass[@]}" "$bin remote ${3:--} $name"
    echo "remote-lab: session $name (prefix e in the lab to go there)"
    ;;
holds) "$bin" hold ;;
stop) t kill-server 2>/dev/null || true ;;
*)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
