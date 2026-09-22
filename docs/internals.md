# Internals

How the agent-aware tmux setup works, for changing it. For using it, see
[guide.md](guide.md).

## The idea

Agents report, tmux draws. Claude Code and Codex call `agents/bin/agent-hook`
on their lifecycle events; the hook writes facts into **pane options** of the
agent's pane (`@agent_state`, `@agent_needs`, ...). The tmux config turns those
options into the bar and the borders with formats, so nothing polls: tmux
redraws when an option changes. Everything else (notifications, sounds,
spaces, mission control) reads the same options.

## Files

| file | role |
| --- | --- |
| `tmux.conf` | general options and keys; sources the two files below, then TPM |
| `theme.conf` | colors (Catppuccin Mocha), status rows, window tabs, borders, menus, popups; the templates of the space line |
| `agents/agents.conf` | agent formats (glyphs, labels, task names), hooks (seen/unseen, re-checks, relayout), keys |
| `agents/install` | registers `agent-hook` in every Claude Code profile and in Codex |
| `agents/bin/agent-hook` | the hook: event JSON on stdin → pane options, notifications, sounds |
| `agents/bin/agent-lib.sh` | shared helpers: process tree, Hyprland, focused client, per-client formats, visibility |
| `agents/bin/agent-reconcile` | repairs states no hook reported (Esc, denied permission, killed agent) |
| `agents/bin/agent-notify` | desktop notifications over D-Bus (uv script, jeepney) |
| `agents/bin/agent-sound` | plays a sound with priority and debounce |
| `agents/bin/agent-remind` | the 15-minute reminder |
| `agents/bin/agent-spaces` | spaces: tagging, numbering, the space line, narrow layouts, prefix+S / prefix+Q menus, next/prev/go |
| `agents/bin/agent-board` | mission control (fzf) |
| `agents/bin/agent-sessions` | session search (fzf) |
| `agents/bin/agent-jump`, `agent-next`, `agent-peek` | going to a pane, to the next agent that needs you, peeking |
| `agents/typings/` | type stubs for jeepney (pyright strict) |
| `spaces.conf` | folders → spaces; local, git-ignored |

Runtime state lives outside the repo: `$XDG_RUNTIME_DIR/tmux-agents/` (running
subagents per session, sound debounce, rounds) and
`~/.local/state/tmux-agents/` (debug logs, only with the `debug` file).

## Pane options

| option | set by | meaning |
| --- | --- | --- |
| `@agent` | hook | `claude` or `codex` |
| `@agent_state` | hook, reconcile, focus hook | `ready working needs compacting done idle error` |
| `@agent_needs` | hook | `permission question plan` while `needs` |
| `@agent_needs_id` | hook | the tool call waiting for permission |
| `@agent_since` | hook | when the state changed (epoch) |
| `@agent_tool` | hook | last tool call, `#` doubled for formats |
| `@agent_msg` | hook | last reply or error |
| `@agent_subs`, `@agent_subtypes` | hook | running subagents |
| `@agent_session`, `@agent_transcript`, `@agent_model`, `@agent_mode`, `@agent_profile` | hook | identity |
| `@agent_prev` | hook | state before compacting |
| `@agent_notify_id`, `@agent_notify_pid` | agent-notify | the pane's notification and its waiter |
| `@agent_tests_sound_at` | hook | last "fight like a man" |

## From events to states

| event (Claude / Codex) | state |
| --- | --- |
| SessionStart | `ready` (compaction restarts keep the state) |
| UserPromptSubmit | `working` |
| PreToolUse | `working` (not while waiting for permission) |
| PermissionRequest | `needs`: `question` for AskUserQuestion, `plan` for ExitPlanMode, else `permission` |
| PostToolUse, PostToolUseFailure | `working`; ends `needs` only for the call that asked |
| Notification (Claude) | `needs` for permission prompts and elicitations |
| Elicitation / ElicitationResult (Claude) | `needs question` / `working` |
| PreCompact / PostCompact | `compacting` / back to the previous state |
| Stop | `done`, or `idle` if you are looking at the pane |
| StopFailure (Claude) | `error` |
| SubagentStart / SubagentStop | subagent count; a subagent's own events never change the main state |
| SessionEnd | all options cleared |

The hook only counts the pane's own agent: it walks up its process tree to the
pane's shell and ignores a `claude -p` or `codex exec` run by another agent.
It never prints and always exits 0 (hook output can land in the conversation;
exit 2 would block a tool call).

**No hook for Esc or a denied permission.** Both end the turn silently.
`agent-reconcile` checks the agent's transcript: Claude ends every turn with a
`system/turn_duration` entry, or an interrupted one with `[Request interrupted
by user`; Codex writes `task_complete` or `turn_aborted`. It runs when you
leave a pane that still looks busy (`pane-focus-out`), when mission control or
`prefix u` open, and on config reload. It also clears panes whose agent
process is gone.

**Seen.** `pane-focus-in` turns `done` into `idle` and closes the pane's
notification.

## Visibility

`ag_visibility` answers "is the user looking at this pane?": `visible` (the
pane is in front, in the focused terminal), `session` (its session is on the
focused terminal, another pane in front) or `away`. The focused terminal comes
from Hyprland (`hyprctl activewindow`, mapped to a tmux client through the
process tree), because tmux only learns focus when it changes; tmux's own
focus flag is the fallback.

- Notifications: only `away`.
- Sounds: unless `visible`; errors, won rounds and accepted plans always.
- `done` becomes `idle` at once when `visible`.

## Notifications

`agent-notify` talks to `org.freedesktop.Notifications` directly (libnotify's
`notify-send` goes through the portal here, which drops click actions). It
sends with `replaces_id` so a pane keeps one notification, stores its id and
its own pid in the pane, then waits for `ActionInvoked` (a click: run
`agent-jump`) or `NotificationClosed`. A replacement stops the previous waiter
with SIGTERM. Urgency is `critical` for needs/error/reminder, `normal` for
done; mako decides how long each stays.

## Sounds

`agent-sound <name>`: priority 4 for needs/question/plan/error/reminder, 3 for
a won round, 2 for done and an accepted plan, 1 for tests. Within 2.5 s of the
last sound only a higher priority plays, killing the previous player
(`$XDG_RUNTIME_DIR/tmux-agents/sound.last`, microseconds from
`$EPOCHREALTIME`). Rounds: `UserPromptSubmit` adds the pane to
`round-<server>-<space>`; on `Stop`, if no other agent of the space is busy,
the round ends, and it is won when two or more panes took part.

## Spaces and the bar

`agent-spaces load` (on config load) reads `spaces.conf`, tags each session
(`@space_auto` from its start folder, `@space` when set by hand), numbers the
sessions of each space by name (`@space-index-<space>`), defines the summary
counts per space (`@cnt-<state>-<space>`) and binds `prefix S` and `prefix Q`.

The status rows are `status-format[N] = #{E:@rowN}`. Globally `@row0` is the
space line (`#{E:@fleet-line}`, per session) and `@row1` the window line
(`@window-line`, tmux's default). The space line is built from three
templates in theme.conf (`@fleet-head-tpl`, `@fleet-chips-tpl`,
`@fleet-tail-tpl`) with placeholders the script fills per space.

`agent-spaces layout` (on resize, attach, session switch, and after tagging)
estimates, for each session shown on a terminal, whether its rows fit the
terminal's width. If not, it sets session-level `@row0..@row4` and `status`:
session chips packed into rows by index range, the summary on the last row or
its own, and window tabs packed by window index (`@narrow-tabs` shortens
titles). The summary chooses words or glyphs by comparing its plain length to
`@summary-room`, the room the layout left on its row.

Moving between sessions (`next`, `prev`, `go`, the search) uses the same
list, sorted by name, that the top row draws with `#{S/n:}`.

## Testing

Never test on the live server by attaching extra clients (they resize windows).
Start an isolated server with the real config and point the scripts at it:

```sh
tmux -L lab -f ~/.config/tmux/tmux.conf new-session -d -s demo
TMUX="$(tmux -L lab display -p '#{socket_path}'),0,0" ~/.config/tmux/agents/bin/agent-board --list
```

To see what a client draws, run it inside a pane of a second server and use
`capture-pane -e` on that pane. To drive the hook without spending API calls,
run a fake agent: a copy of `python3` named `claude` (comm must be `claude`)
that feeds JSON events to `agent-hook`; `AG_SOUND_PLAYER` swaps the player for
a logger and `AG_SPACES_FILE` points to a test `spaces.conf`. Check
`agents/bin/agent-notify` with pyright:

```sh
uv run --no-project --with jeepney --with pyright pyright
```

## Pitfalls we hit

- **`status-format` is an array option.** Setting one entry per session
  replaces the whole array for that session (line 1 went blank). Per-session
  content goes in user options (`@rowN`, `@fleet-line`) that global formats
  expand.
- **`display -p -c client` evaluates for the calling client,** not for the
  one named. Use `ag_client` (`list-clients` filtered by name).
- **Session names can be numbers.** `-t 4` means window 4; use `-t '=4:'`.
- **`display-popup` doesn't expand formats** in its command; open popups
  from a script run by `run-shell`, which does.
- **fzf hands `become` processes `/dev/tty` as stdin,** which tmux refuses as
  a client terminal; `agent-peek` duplicates stdout onto stdin instead.
- **`destroy-unattached` on a detached session destroys it at once.**
- **Command size.** A tmux command has a size limit ("command too long");
  long formats go in options and are referenced with `#{E:...}`.
- **bash 5.2 `patsub_replacement`:** `&` in `${x//a/b}` replacements expands
  to the match; `agent-spaces` turns it off.
- **Ubuntu's uutils `date`** ignores `%3N`; use `$EPOCHREALTIME` (its decimal
  separator follows the locale).
- **uutils binaries are multi-call:** a copy of `/bin/sleep` named `claude`
  exits at once.
- **tmux's `#{S:}` loop runs in creation order;** `#{S/n:}` sorts by name.
- **gitleaks' default allowlist** ignores `/home/...` matches; the
  home-path rule reports the username only.
