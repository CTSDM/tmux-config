# tmux config

Personal tmux setup (prefix `Alt+a`) that keeps track of the Claude Code and
Codex sessions running in its panes, inspired by
[cmux](https://github.com/manaflow-ai/cmux) but built on plain tmux.

## Install

```sh
git clone <this repo> ~/.config/tmux
~/.config/tmux/setup.sh
```

`setup.sh` installs TPM and the plugins, enables the commit leak guard and runs
`agents/install`, which registers the agent hook with every Claude Code profile
(`~/.claude`, `~/.config/claude/*/`) and with Codex (`~/.codex/hooks.json`).
`agents/install --uninstall` removes it again. Needs `jq`, `python3`, `fzf`, `uv`, a
notification daemon (mako, dunst, ...) and, for sounds, PipeWire's `pw-play`.

## Documentation

- [docs/guide.md](docs/guide.md): what you see and how to use it: the bar,
  agent states, notifications, sounds, spaces, moving between sessions,
  mission control, troubleshooting.
- [docs/internals.md](docs/internals.md): how it works, for changing it,
  including the pitfalls we hit.

At a glance: a two-row bar (your space's sessions, then this session's
windows) with a glyph per agent (`●` working, `▲` needs you, `✓` done, `○`
idle), desktop notifications and sounds when an agent needs you, personal and
work spaces that never mix, and `prefix a` / `prefix u` / `prefix e` to get
anywhere.

## Development

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
and email addresses) and refuse any word listed in `.git/info/private-words`
(one per line, never committed).
