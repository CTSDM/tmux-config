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
`agents/install --uninstall` removes it again. Needs `jq`, `fzf`, and for desktop
notifications `python3-gi` with a notification daemon (mako, dunst, ...).

## What you get

**Status bar** (two lines, top): your session's windows, each with the state
of its agents and the agent's task as the window name; below it, every session
that has agents, with totals on the right.

| glyph | meaning |
| --- | --- |
| `●` | working |
| `▲` | needs you: a permission, a question or a plan to approve |
| `✓` | done, and you have not looked at it yet |
| `○` | idle |
| `◐` | the turn ended but subagents are still running |
| `↻` | compacting context |
| `✗` | stopped by an error (rate limit, API error...) |
| `◇` | an agent that is not reporting (started before the hooks) |
| `+N` | running subagents |

**Pane borders** take the state's color. In split windows each pane's bottom
border also shows a label such as `claude:work · needs permission · Bash: git push`.

**Keys**

| key | action |
| --- | --- |
| `prefix a` | mission control: every pane, agents first, live preview. `enter` go there, `ctrl-o` peek (work in it from the popup), `ctrl-a` agents only / all, `ctrl-r` refresh |
| `prefix u` | jump to the agent that needs you most |
| `prefix a` inside a peek | close the peek |
| `prefix R` | reload the config |

Jumping to a session that is open in another terminal window focuses that
window (Hyprland) instead of taking over the current one.

**Desktop notifications** go out when an agent needs you, finishes or fails
while you are not looking at its session. Clicking one jumps to the pane;
looking at the pane closes it.

## How it works

- `agents/bin/agent-hook` runs on the agents' lifecycle hooks and writes facts
  into pane options (`@agent_state`, `@agent_needs`, `@agent_subs`,
  `@agent_tool`, ...). It prints nothing and always exits 0.
- `agents/agents.conf` turns those options into glyphs, labels and counts with
  tmux formats; nothing polls. `theme.conf` (Catppuccin Mocha) places them:
  a solid two-line bar, state-colored borders, matching popups and menus.
- Denying a permission or pressing Esc ends a turn without any hook, so
  `agent-reconcile` checks the agent's transcript when you leave a pane that
  still looks busy, and when mission control opens.
- `touch ~/.local/state/tmux-agents/debug` logs every hook event (including
  prompts) and script errors to that directory.

## Privacy guard

Commits run gitleaks (`.gitleaks.toml`: default rules plus absolute home paths
and email addresses) and refuse any word listed in `.git/info/private-words`
(one per line, never committed).
