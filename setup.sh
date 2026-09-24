#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# Tmux + Tmuxifier setup script
# Installs tmux config, TPM (plugin manager), the Claude Code / Codex agent
# hooks, and tmuxifier with layouts.
#
# Usage:
#   ./setup.sh                  # install everything
#   ./setup.sh --no-layouts     # skip copying tmuxifier layouts
#
# Assumes:
#   - tmux is already installed
#   - git is available
#   - This repo is cloned to ~/.config/tmux (or gets linked there)
#   - jq is available (for the agent hooks)
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKIP_LAYOUTS=false

for arg in "$@"; do
    case "$arg" in
        --no-layouts) SKIP_LAYOUTS=true ;;
        -h|--help)
            echo "Usage: $0 [--no-layouts]"
            echo "  --no-layouts  Skip copying tmuxifier session layouts"
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
    if cargo build --release --locked --manifest-path "$SCRIPT_DIR/agentd/Cargo.toml" \
        --target-dir "$SCRIPT_DIR/agentd/target"; then
        mkdir -p "$(dirname "$AGENTD")"
        # A new file renamed over the old one: a running daemon keeps its inode.
        tmp="$(dirname "$AGENTD")/.agentd.$$"
        cp "$SCRIPT_DIR/agentd/target/release/agentd" "$tmp"
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
command -v uv &>/dev/null || warn "uv is not installed: desktop notifications (agents/bin/agent-notify) need it."

# --- 4. Tmuxifier -----------------------------------------------------------

TMUXIFIER_DIR="$HOME/.tmuxifier"
info "Installing tmuxifier"

if [ -d "$TMUXIFIER_DIR" ]; then
    ok "Tmuxifier already installed at $TMUXIFIER_DIR"
else
    git clone https://github.com/jimeh/tmuxifier.git "$TMUXIFIER_DIR"
    ok "Tmuxifier installed"
fi

# --- 5. Tmuxifier layouts ----------------------------------------------------

LAYOUTS_SRC="$SCRIPT_DIR/layouts"
LAYOUTS_DST="$TMUXIFIER_DIR/layouts"

if [ "$SKIP_LAYOUTS" = true ]; then
    info "Skipping layout copy (--no-layouts)"
elif [ -d "$LAYOUTS_SRC" ]; then
    info "Copying tmuxifier session layouts"
    mkdir -p "$LAYOUTS_DST"
    cp "$LAYOUTS_SRC"/*.session.sh "$LAYOUTS_DST/" 2>/dev/null || true
    ok "Layouts copied to $LAYOUTS_DST"
else
    warn "No layouts directory found at $LAYOUTS_SRC — skipping"
    warn "If your layouts are in ~/.tmuxifier/layouts, they are already in place."
fi

# --- 6. Shell integration ----------------------------------------------------

info "Checking shell integration for tmuxifier"

SHELL_RC=""
case "$(basename "${SHELL:-bash}")" in
    zsh)  SHELL_RC="$HOME/.zshrc" ;;
    bash) SHELL_RC="$HOME/.bashrc" ;;
    fish) SHELL_RC="$HOME/.config/fish/config.fish" ;;
    *)    SHELL_RC="$HOME/.profile" ;;
esac

TMUXIFIER_INIT='eval "$(~/.tmuxifier/bin/tmuxifier init -)"'

if [ -f "$SHELL_RC" ] && grep -qF 'tmuxifier init' "$SHELL_RC"; then
    ok "Tmuxifier init already present in $SHELL_RC"
else
    info "Adding tmuxifier init to $SHELL_RC"
    {
        echo ""
        echo "# Tmuxifier"
        echo "$TMUXIFIER_INIT"
    } >> "$SHELL_RC"
    ok "Added tmuxifier init to $SHELL_RC"
fi

# --- Done --------------------------------------------------------------------

echo ""
ok "Setup complete!"
echo ""
echo "  Next steps:"
echo "    1. Restart your shell or run: source $SHELL_RC"
echo "    2. Open tmux. Plugins should already be installed."
echo "       If not, press Alt+a then I to install them."
echo "    3. Use tmuxifier: tmuxifier list-sessions"
echo ""
