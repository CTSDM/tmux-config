#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# Tmux setup script
# Installs the tmux config, TPM (plugin manager), agentd (and its desktop
# bridge, a systemd user service) and the Claude Code / Codex agent hooks.
#
# Usage:
#   ./setup.sh
#
# Assumes:
#   - Linux (the agent layer reads /proc); any desktop, see "Desktop" below
#   - tmux 3.7 or later is already installed
#   - git is available
#   - This repo is cloned to ~/.config/tmux (or gets linked there)
#   - jq is available (for the agent hooks)
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for arg in "$@"; do
    case "$arg" in
        -h|--help)
            echo "Usage: $0"
            exit 0
            ;;
        *) echo "Unknown option: $arg"; exit 1 ;;
    esac
done

info()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m::\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m::\033[0m %s\n' "$*"; }
error() { printf '\033[1;31m::\033[0m %s\n' "$*"; }

# --- Preflight checks -------------------------------------------------------

if ! command -v tmux &>/dev/null; then
    error "tmux is not installed. Install it first, then re-run this script."
    exit 1
fi

if ! command -v git &>/dev/null; then
    error "git is not installed. Install it first, then re-run this script."
    exit 1
fi

# The agent layer reads /proc (agents' processes, their shells).
if [ "$(uname -s)" != Linux ]; then
    error "The agent layer needs Linux (it reads /proc). Nothing installed."
    exit 1
fi

tmux_version="$(tmux -V | sed -n 's/^tmux \([0-9]*\.[0-9]*\).*/\1/p')"
if [ -n "$tmux_version" ] && [ "$(printf '%s\n' 3.7 "$tmux_version" | sort -V | head -n1)" != 3.7 ]; then
    warn "tmux $tmux_version: 3.7 or later is needed; older versions leak memory while they redraw the bar."
fi

for cmd in fzf python3; do
    command -v "$cmd" &>/dev/null || warn "$cmd is not installed: the popups (prefix a, e, u...) need it."
done

# --- Desktop: notifications, sounds, focus ----------------------------------
# Any Linux desktop works (Hyprland, sway, i3, GNOME, KDE...): nothing here is
# required, this only says what will be missing.

if ! command -v busctl &>/dev/null || ! busctl --user list &>/dev/null; then
    info "Notifications: no session bus to check from here (over ssh?)."
elif busctl --user list 2>/dev/null | grep -q '^org\.freedesktop\.Notifications '; then
    ok "Notifications: a notification server is on the session bus."
else
    warn "Notifications: no notification server (mako, dunst, your desktop's own...): no desktop notifications."
fi

if ! command -v pw-play &>/dev/null && ! command -v ffplay &>/dev/null \
    && ! command -v mpv &>/dev/null && ! command -v aplay &>/dev/null; then
    warn "Sounds: none of pw-play, ffplay, mpv or aplay is installed: no sounds."
fi

if [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
    info "Not on Hyprland: which pane you're looking at comes from your terminal's focus events, and clicking a notification shows the pane in your last used terminal instead of raising the window that already has it."
fi

# --- 1. Tmux configuration --------------------------------------------------

TMUX_CONFIG_DIR="$HOME/.config/tmux"
info "Setting up tmux config at $TMUX_CONFIG_DIR"

# The config is more than tmux.conf now (agents/), so the repo itself lives there.
if [ "$SCRIPT_DIR" = "$TMUX_CONFIG_DIR" ]; then
    ok "Config already in place"
elif [ -e "$TMUX_CONFIG_DIR" ]; then
    error "$TMUX_CONFIG_DIR already exists. Clone this repo there, or move it away and re-run."
    exit 1
else
    mkdir -p "$(dirname "$TMUX_CONFIG_DIR")"
    ln -s "$SCRIPT_DIR" "$TMUX_CONFIG_DIR"
    ok "Linked $TMUX_CONFIG_DIR -> $SCRIPT_DIR"
fi

# Leak guard for commits (gitleaks + .git/info/private-words), see .githooks/
git -C "$SCRIPT_DIR" config core.hooksPath .githooks
command -v gitleaks &>/dev/null || warn "gitleaks is not installed: commits will be refused until it is."

# --- 2. TPM (Tmux Plugin Manager) -------------------------------------------

TPM_DIR="$TMUX_CONFIG_DIR/plugins/tpm"
info "Installing TPM (Tmux Plugin Manager)"

if [ -d "$TPM_DIR" ]; then
    ok "TPM already installed at $TPM_DIR"
else
    git clone https://github.com/tmux-plugins/tpm "$TPM_DIR"
    ok "TPM installed"
fi

# --- 3. Install tmux plugins via TPM ----------------------------------------

info "Installing tmux plugins"
if [ -f "$TPM_DIR/bin/install_plugins" ]; then
    "$TPM_DIR/bin/install_plugins"
    ok "Plugins installed"
else
    warn "Could not auto-install plugins. Open tmux and press prefix + I to install."
fi

# --- 3a. agentd (the agent layer's daemon, in Rust) ----------------------------

AGENTD="$HOME/.local/bin/agentd"
AGENTD_OFF="${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agents/agentd.off"
agentd_ok=false
if ! command -v cargo &>/dev/null; then
    warn "cargo is not installed: agentd not built, the agent hooks stay on bash."
else
    info "Building agentd"
    # Static and not position independent on glibc: no dynamic loader or
    # relocations at start, half of every hook's cost (~1.1 → ~0.6 ms). An
    # explicit --target keeps the flags off the proc macros, which need PIC.
    host="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p' || true)"
    build=(cargo build --release --locked --manifest-path "$SCRIPT_DIR/agentd/Cargo.toml"
        --target-dir "$SCRIPT_DIR/agentd/target")
    built="$SCRIPT_DIR/agentd/target/release/agentd"
    if [[ "$host" == *-linux-gnu ]] && "${build[@]}" --target "$host" \
        --config "target.$host.rustflags=['-C','target-feature=+crt-static','-C','relocation-model=static']"; then
        built="$SCRIPT_DIR/agentd/target/$host/release/agentd"
    elif ! "${build[@]}"; then
        built=""
    fi
    if [ -n "$built" ]; then
        mkdir -p "$(dirname "$AGENTD")"
        # A new file renamed over the old one: a running daemon keeps its inode.
        tmp="$(dirname "$AGENTD")/.agentd.$$"
        cp "$built" "$tmp"
        chmod 755 "$tmp"
        mv -f "$tmp" "$AGENTD"
        agentd_ok=true
        ok "agentd installed at $AGENTD"
        warn "Running agentd daemons keep the old binary until 'agentd ctl stop' in their tmux server."
    else
        warn "agentd did not build: the agent hooks stay on bash."
    fi
fi
[ -e "$AGENTD_OFF" ] && warn "agentd is switched off ($AGENTD_OFF): the agent hooks stay on bash."

# --- 3a'. The desktop bridge (tmux on servers you reach over ssh) --------------
# An idle process on a private socket: nothing reaches it unless an ssh
# connection forwards it (docs/guide.md, "On a remote server").

BRIDGE_UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/agentd-bridge.service"
if [ "$agentd_ok" != true ]; then
    :
elif ! systemctl --user show-environment &>/dev/null; then
    info "No systemd user session: to get notifications from tmux on your servers, run '$AGENTD bridge' when you log in."
else
    mkdir -p "$(dirname "$BRIDGE_UNIT")"
    cat > "$BRIDGE_UNIT" <<'UNIT'
[Unit]
Description=agentd bridge: notifications and sounds from tmux servers over ssh

[Service]
ExecStart=%h/.local/bin/agentd bridge
Restart=on-failure

[Install]
WantedBy=default.target
UNIT
    systemctl --user daemon-reload
    systemctl --user enable agentd-bridge.service &>/dev/null
    # A new binary: the running bridge picks it up.
    systemctl --user restart agentd-bridge.service
    ok "Desktop bridge running (agentd-bridge.service). For a server, in ~/.ssh/config under its Host:"
    echo "      RemoteForward /run/user/<uid on the server>/agentd-bridge.sock /run/user/%i/tmux-agents/desktop.sock"
fi

# --- 3b. Agent hooks (Claude Code, Codex) ------------------------------------

info "Registering the agent hooks with Claude Code and Codex"
if command -v jq &>/dev/null; then
    if [ "$agentd_ok" = true ] && [ ! -e "$AGENTD_OFF" ]; then
        "$TMUX_CONFIG_DIR/agents/install" --agentd "$AGENTD"
    else
        "$TMUX_CONFIG_DIR/agents/install" --bash
    fi
else
    warn "jq is not installed: skipped. Install it, then run agents/install."
fi

# --- 3c. Spaces and sounds -----------------------------------------------------

if [ ! -f "$TMUX_CONFIG_DIR/spaces.conf" ]; then
    cp "$TMUX_CONFIG_DIR/spaces.example" "$TMUX_CONFIG_DIR/spaces.conf"
    warn "Created spaces.conf from the example: list your personal and work folders there."
fi
SOUNDS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tmux-agents/sounds"
mkdir -p "$SOUNDS_DIR"
ok "Agent sounds go in $SOUNDS_DIR (names in agents/bin/agent-sound)"
command -v uv &>/dev/null || warn "uv is not installed: the bash implementation's notifications (agents/bin/agent-notify) and the tests need it."

# --- Done --------------------------------------------------------------------

echo ""
ok "Setup complete!"
echo ""
echo "  Next steps:"
echo "    1. Open tmux, or reload its config if it runs (Alt+a then R)."
echo "    2. Plugins should already be installed. If not, press Alt+a then I."
echo "    3. Agents started before now report once they restart."
echo ""
