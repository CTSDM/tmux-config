# agent-lib.sh: helpers shared by the agents/bin scripts. Source it, don't run it.

AG_BIN=$(cd "${BASH_SOURCE[0]%/*}" && pwd)
AG_RUN=${XDG_RUNTIME_DIR:-/tmp}/tmux-agents                  # running subagents, per session
AG_STATE=${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agents   # debug logs (touch $AG_STATE/debug)
US=$'\x1f'   # field separator for tmux output: unlike a tab, empty fields survive `read`

[[ -e $AG_STATE/debug ]] && exec 2>>"$AG_STATE/errors.log"

# Every pane option agent-hook writes. SessionEnd and agent-reconcile clear them all.
AG_OPTS=(@agent @agent_session @agent_profile @agent_model @agent_mode @agent_state
  @agent_needs @agent_needs_id @agent_since @agent_prev @agent_tool @agent_msg
  @agent_subs @agent_subtypes @agent_transcript @agent_tests_sound_at)

# Parent pid of a process.
ag_ppid() {
  local stat rest
  read -r stat <"/proc/$1/stat" 2>/dev/null || return 1
  rest=${stat##*) }
  rest=${rest#* }
  printf '%s' "${rest%% *}"
}

# hyprctl with a timeout. Finds the running Hyprland instance when the
# inherited signature is stale (e.g. a tmux server older than the session).
ag_hyprctl() {
  command -v hyprctl >/dev/null || return 1
  local dir=${XDG_RUNTIME_DIR:-/run/user/$UID}/hypr sig=${HYPRLAND_INSTANCE_SIGNATURE:-}
  if [[ -z $sig || ! -S $dir/$sig/.socket.sock ]]; then
    sig=$(ls -t "$dir" 2>/dev/null | head -n1)
    [[ -n $sig ]] || return 1
  fi
  HYPRLAND_INSTANCE_SIGNATURE=$sig timeout 2 hyprctl "$@"
}

# Succeeds when pid $2 is pid $1 or one of its ancestors.
ag_descends_from() {
  local pid=$1
  for _ in {1..8}; do
    [[ $pid == "$2" ]] && return 0
    pid=$(ag_ppid "$pid") || return 1
    ((pid > 1)) || return 1
  done
  return 1
}

# Prints the tmux client whose terminal window has keyboard focus. Asks
# Hyprland when it can (tmux only learns about focus when it changes), and
# falls back to tmux's own focus flag.
ag_focused_client() {
  local clients name pid flags win
  clients=$(tmux list-clients -F "#{client_name}$US#{client_pid}$US#{client_flags}") || return 1
  win=$(ag_hyprctl activewindow -j 2>/dev/null | jq -r '.pid // empty' 2>/dev/null)
  while IFS=$US read -r name pid flags; do
    if [[ -n $win ]]; then
      ag_descends_from "$pid" "$win" && { printf '%s\n' "$name"; return 0; }
    elif [[ ,$flags, == *,focused,* ]]; then
      printf '%s\n' "$name"
      return 0
    fi
  done <<<"$clients"
  return 1
}

# Prints whether the user sees a pane: "visible", "session" (its session is on
# screen but another pane or window is in front) or "away".
ag_visibility() { # <pane>
  local sess win_active pane_active client
  IFS=$US read -r sess win_active pane_active < <(
    tmux display -p -t "$1" "#{session_name}$US#{window_active}$US#{pane_active}") || { echo away; return; }
  if client=$(ag_focused_client) && [[ $(tmux display -p -c "$client" '#{client_session}') == "$sess" ]]; then
    [[ $win_active == 1 && $pane_active == 1 ]] && echo visible || echo session
  else
    echo away
  fi
}

# Focuses the terminal window that hosts a tmux client (Hyprland only).
ag_raise_client() {
  local pid wins
  pid=$(tmux display -p -c "$1" '#{client_pid}' 2>/dev/null) || return 1
  wins=$(ag_hyprctl clients -j 2>/dev/null | jq -r '.[].pid' 2>/dev/null) || return 1
  for _ in {1..8}; do
    if grep -qx "$pid" <<<"$wins"; then
      ag_hyprctl dispatch focuswindow "pid:$pid" >/dev/null
      return 0
    fi
    pid=$(ag_ppid "$pid") || return 1
    ((pid > 1)) || return 1
  done
  return 1
}

# Succeeds when the space's alerts are muted (prefix+Q, server option
# @agent_mute_<space> set to on): no notifications or sounds from its agents.
ag_space_muted() { # <space>
  [[ -n $1 && $(tmux show -gqv "@agent_mute_${1//[^A-Za-z0-9_-]/_}") == on ]]
}

# Task name from a pane title: Claude sets "✳ topic", Codex "topic | project".
ag_title() {
  local t=${1#✳ }
  printf '%s' "${t% | *}"
}
