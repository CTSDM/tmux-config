# tmux config

Personal tmux setup (prefix `Alt+a`) that keeps track of the Claude Code and
Codex sessions running in its panes, inspired by
[cmux](https://github.com/manaflow-ai/cmux) but built on plain tmux.

## Install

```sh
git clone <this repo> ~/.config/tmux
~/.config/tmux/setup.sh
```

`setup.sh` installs TPM and the plugins, enables the commit leak guard, builds
`agentd` (the agent layer's daemon, in Rust) into `~/.local/bin/agentd` and
runs `agents/install`, which registers the agent hook with every Claude Code
profile (`~/.claude`, `~/.config/claude/*/`) and with Codex
(`~/.codex/hooks.json`): `agentd hook`, or the bash `agent-hook` when agentd
could not be built or is switched off (`agentd.off`, see the guide).
`agents/install --uninstall` removes it again. It also creates `spaces.conf`
from `spaces.example` (your folders, never committed) and the folder for the
sounds.

Needs Linux, tmux 3.7 or later, `git`, `jq`, `python3`, `fzf`, `uv`, a
notification server and a sound player (see Desktop below), and for agentd a
Rust toolchain (`cargo`); committing needs `gitleaks`. tmux 3.6 leaks memory
while it redraws this bar. tmux 3.7c loses a popup's top rows when a pane
under it prints: build it from the release tarball with
`tests/upstream/tmux-3.7c-popup-overlay.patch` (see
[tests/upstream/README.md](tests/upstream/README.md)).

### Desktop

Any Linux desktop works (Hyprland, sway, i3, GNOME, KDE...); no launcher such
as rofi is involved, and `setup.sh` says what your desktop will be missing.

- **Notifications** go through the standard D-Bus notification service, so any
  notification server shows them (mako, dunst, your desktop's own...).
  Clicking one jumps to its pane when the server runs the notification's
  default action on a click: mako does; dunst does with
  `mouse_left_click = do_action, close_current`.
- **Sounds** play with `pw-play`, else `ffplay`, `mpv` or `aplay`.
- **Hyprland** adds two things: whether you're looking at a pane is asked of
  it (elsewhere it comes from the terminal's focus events, which kitty, foot,
  alacritty and wezterm send), and a click on a notification raises the
  terminal window that already shows that session (elsewhere the pane is shown
  in the terminal you used last).

## Documentation

- [docs/guide.md](docs/guide.md): what you see and how to use it: the bar,
  agent states, notifications, sounds, spaces, moving between sessions,
  mission control, troubleshooting.
- [docs/internals.md](docs/internals.md): how it works, for changing it,
  including agentd and the pitfalls we hit.
- [docs/daemon/](docs/daemon/): agentd's design, the behavior contract both
  implementations keep, and its task log.

At a glance: a two-row bar (your space's sessions, then this session's
windows) with a glyph per agent (`●` working, `▲` needs you, `✓` done, `○`
idle), desktop notifications and sounds when an agent needs you, personal and
work spaces that never mix, and `prefix a` / `prefix u` / `prefix e` to get
anywhere.

## Development

agentd's checks (format, lints, unit and integration tests against isolated
tmux servers, dependency audit):

```sh
agentd/check.sh
```

The contract suite runs the same black-box tests against either
implementation (`tests/README.md`):

```sh
cd tests && uv run pytest                                  # bash
AGENT_IMPL=rust AGENTD=../agentd/target/release/agentd uv run pytest
```

Codex integration tests (isolated tmux server, no API calls):

```sh
python3 -B agents/tests/test_codex.py -v
```

Shell scripts are plain bash. `agents/bin/agent-notify` is a uv script
(dependencies declared inline) checked with pyright in strict mode, using the
stubs in `agents/typings`:

```sh
uv run --no-project --with jeepney --with pyright pyright
```

## Privacy guard

Commits run gitleaks (`.gitleaks.toml`: default rules plus absolute home paths
and email addresses other than no-reply ones) and refuse any word listed in
`.git/info/private-words` (one per line, never committed): `pre-commit` checks
the staged changes and file names, `commit-msg` the commit message.
