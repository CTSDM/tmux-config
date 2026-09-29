# Guide

What this tmux setup does and how to use it. For how it works inside, see
[internals.md](internals.md).

The prefix is `Alt+a`. "prefix x" means: press `Alt+a`, release, press `x`.

## The bar

Two rows at the top, on their own colors so they never blend into the panes:

```
 personal │ 1 4   2 japanese ◇◇◇○  3 tmux-ricing   4 vco-pfm ◐+3      1 working - 1 idle
 tmux-ricing   1 ● Tmux configuration upd…   2 zsh                                 00:22
```

**Row 1, your space.** The space's name (`personal`), then every session of
the space, numbered, each followed by one glyph per agent in it. The session
you are in is a raised chip, shown by name only: its agents are on row 2. On the
right, what the *other* sessions of the space are doing, most urgent first and
only the states present: `1 needs you - 1 done - 2 working - 3 idle`. When the
words don't fit, the same in glyphs: `▲1 ✓1 ●2 ○3`. Nothing from other spaces
ever shows here.

**Row 2, this session.** The session as a pill in its space's color, then its
windows. Each tab shows the glyphs of its agents and the task the agent gave
itself as the window's name (unless you renamed the window). The current
window is a raised tab. On the right: `prefix` while the prefix key is
active, `sounds off` when sounds are off, and the clock.

**Pane borders** take the color of their agent's state, a "ring" you notice
from the corner of your eye. In a window with several panes, each pane's bottom
border also gets a label, e.g. `● claude:work working (1 Explore) · Bash: npm test`.

### Narrow terminals

On a terminal too narrow for that (a vertical screen), the bar grows to up to
5 rows instead of cutting things off: the sessions of the space continue on
the next row, aligned under the first ones, with the summary on the last. If
the windows don't fit either, they wrap too, with shorter titles and without
the session pill (the raised chip on row 1 already marks the session) and the
clock. Each terminal gets the layout that fits its own width; it is recomputed
when you resize, attach or switch sessions.

## Agent states

| glyph | state | meaning |
| --- | --- | --- |
| `●` | working | thinking or running tools |
| `▲` | needs you | waiting for a permission, an answer to a question, or your approval of a plan |
| `✓` | done | finished, and you haven't looked at it yet |
| `○` | idle | finished and seen, or not started yet |
| `◐` | background | its turn ended but subagents it started are still running |
| `○゙` `◐゙` | background | the dakuten (the two strokes): local shell executions it started are still running (Claude and Codex), with or without subagents |
| `↻` | compacting | summarizing its context |
| `✗` | error | its turn stopped on an API error or a rate limit |
| `◇` | untracked | a Claude or Codex that doesn't report (see below) |
| `+N` | | subagents running |

Colors: yellow working, peach needs you, green done, red error, teal
compacting, grey idle. The space's color (mauve for personal, blue for work)
only ever marks where you are.

`done` turns into `idle` when you look at the pane (or at once, if it finished
while you were looking at it): that's how the bar tells you what you haven't
seen yet. Two levels stand out from the rest:

- **Needs you (`▲`):** the session's name and the window's tab blink like a
  car's turn signal. Act on it.
- **Finished, not seen (`✓`):** the session's name and the window's tab turn
  green until you look at the pane. Something to read. For the first 2
  minutes after it finishes, a soft green band sweeps across them, at half
  the speed of the amber one; then the green stays still. To change how
  long it moves: `set -g @agent_unseen_blink_for 300` (seconds; `0` = until
  you look).

Amber wins over green: a session with one agent that needs you and another
that finished shows the amber band.

Claude Code says nothing when you allow a command in its permission dialog.
With agentd, the `▲` turns `●` within about two seconds of the command
starting (a Bash command; other tools stay `▲` until they return, usually at
once).

Codex distinguishes a blocking question (`needs question`) from a permission
request. Its interruptions become idle immediately, and terminal API/quota errors
appear within about two seconds, even if you stay in the pane. Local commands that
outlive the turn keep the background indicator until they exit.

The border can also show `plan mode` and `question sent`. Plan mode means the
agent is planning, not necessarily waiting for approval. An asynchronous question
can coexist with ongoing work; its badge is dismissed by your next message, not
by the tool's acknowledgement that the question was sent.

**Untracked agents (`◇`).** An agent reports its state through hooks, which
Claude Code and Codex only read when they start. An agent started before the
hooks were installed shows as `◇` until you restart it (`claude --resume`,
`codex resume`). The first time Codex starts with the hooks it asks you to
review them ("Hooks need review"): trust them, or they won't run.
After adding the Codex `Interrupt` hook, restart/resume Codex and review the new
entry with `/hooks`. Existing hook definitions do not need to be replaced.

## Noticing when an agent needs you

The signals stack, from the quietest to the loudest:

1. **The bar:** a `▲` next to the session, `1 needs you` in bold at the top
   right, and the session's name and the window's tab blink like a modern
   car's turn signal: an amber band sweeps across the name, stays lit, goes
   dark, and again, about once a second, until you answer.
2. **The pane border** turns peach.
3. **A desktop notification**, when you are not looking at that session.
4. **A sound** ("Need backup!"), when you are not looking at that pane.
5. **A reminder** after 15 minutes without an answer: the sound "Come out,
   come out, wherever you are…" and the notification again.

`prefix u` then takes you straight to it.

## Notifications

A desktop notification goes out when an agent:

| event | title | text | stays on screen |
| --- | --- | --- | --- |
| needs your permission | `session · task` | `Needs permission: Bash: git push` | until you act |
| has a question | `session · task` | `Has a question for you` | until you act |
| has a plan for you | `session · task` | `Plan ready for your review` | until you act |
| finished | `session · task` | the start of its last reply | 6 seconds |
| stopped on an error | `session · task` | `Stopped: rate_limit` | until you act |
| still waits after 15 min | `session · task` | `Still waiting for you, 15 min now` | until you act |

- **Only when you are away:** the terminal window showing that session doesn't
  have the focus (another kitty window or another app does). If you are
  looking at the session, the bar and the borders are enough.
- **"Until you act"** comes from mako: those are sent as critical, and your
  mako config keeps critical notifications until dismissed. The others use
  mako's default timeout (6 s).
- **Clicking a notification** takes you to the pane: it focuses the kitty
  window that already shows that session, or switches your most recent
  terminal to it.
- **Looking at the pane closes its notification.** Each pane has at most one:
  a new one replaces the previous.
- **Finished while subagents still run:** no notification until they are done
  and the agent really finishes.
- **Finished with a shell still running in the background:** the notification
  goes out (a dev server may run forever) and says so: `… · 1 shell still
  running`. When the shell ends, Claude usually picks up by itself.
- **Muting:** `prefix Q` → "Mute work" (or personal) silences that space's
  notifications and sounds; its bar shows `muted`. The bar keeps updating.
- **mako's privacy mode** hides every notification; switch back to the normal
  config to see them.

## Sounds

Put the sound files in `~/.local/share/tmux-agents/sounds/`, named after the
moment (`.wav`, `.ogg`, `.oga`, `.mp3` or `.flac`). A missing file stays silent.

| file | when |
| --- | --- |
| `need-backup` | an agent needs your permission |
| `report-in` | an agent has a question |
| `wait-for-my-go` | a plan is ready for your review |
| `lets-do-this` | you accepted a plan |
| `enemy-down` | an agent finished |
| `ct-win` | the last busy agent of a round finished (two or more took part) |
| `oh-man` | an agent stopped on an error or a rate limit |
| `come-to-papa` | an agent has waited for you for 15 minutes |
| `fight-like-a-man` | an agent started a test run |

- They stay quiet for the pane you are looking at. Errors, a won round and an
  accepted plan always play.
- A round is the set of agents of a space that worked since it was last quiet;
  when the last of them finishes and at least two took part, "Counter-Terrorists
  win" plays instead of "Enemy down".
- Test runs are recognized from the command (`npm test`, `pytest`, `go test`,
  `cargo test`, `vitest`...) and play at most once every 10 minutes per pane.
- Several sounds within 2.5 s: only the most important plays, and it cuts off a
  less important one already playing.
- `prefix Q` → "Turn sounds off" silences them all; the bar shows `sounds off`.
- Settings (in tmux.conf or `prefix :`): `set -g @agent_sound_volume 0.6`
  (0 to 1), `set -g @agent_remind_after 900` (seconds), `set -g
  @agent_test_regex '...'` (which commands count as tests).

## Spaces

Personal and work sessions live in one tmux but never mix. `spaces.conf`
(next to tmux.conf, never committed) says which folders belong to which space:

```
personal ~/repos/github.com/<me>
work     ~/repos/github.com/<employer>
personal *
```

First match wins; a folder covers its subfolders; `*` catches every other
folder. A session belongs to the space of the folder it was started in.
`prefix S` changes it by hand ("from its folder" goes back). After editing
spaces.conf, reload with `prefix R`.

Spaces are strict: the bar, moving between sessions, the search, mission
control and `prefix u` only ever show and reach the sessions of the space you
are in. The only exceptions are the explicit "all spaces" views (`ctrl-s` in
the search and in mission control). Notifications and sounds come from every
space, unless you mute one.

## Moving around

| keys | what it does |
| --- | --- |
| `prefix )` / `prefix (` | next / previous session of your space, in the top row's order (keep pressing to go on) |
| `prefix g` then `1`-`9` | the session with that number in the top row |
| `prefix e` | search the sessions of your space: type to filter, `enter` switches this terminal; `ctrl-s` for all spaces |
| `prefix L` | back to the session you were in before |
| `prefix N` | a new session (form below) |
| `prefix u` | the agent of your space that needs you most: waiting on you (longest first), then errors, then finished work you haven't seen |
| `prefix a` | mission control (below) |
| `prefix S` | this session's space |
| `prefix Q` | alerts: sounds on/off, mute a space |
| `prefix R` | reload the config |

Jumping to a session that another kitty window already shows focuses that
window rather than taking over yours (`prefix u`, mission control, a clicked
notification). Moving with `( ) g e` switches the terminal you are in.

## A new session

`prefix N` opens a small form:

```
  in personal · Tab completes the path · ctrl-c cancels

  session name › grammar
  path         › ~/repos/github.com/<me>/japan-grammar

  → grammar in ~/repos/…/japan-grammar · personal
    enter creates it · any other key cancels
```

- **path** starts at the folder of the pane you are in; Tab completes it like
  the shell (case-insensitive, first Tab lists the options). A folder that
  doesn't exist can be created.
- **session name** left empty takes the folder's name. If a session with that
  name exists, the form just opens it.
- **Space:** the one `spaces.conf` names for the folder, or else the space you
  are in (not the `*` default). The last line says which, and warns when the
  folder belongs to another space.
- Enter creates it and switches this terminal to it.

## Mission control

`prefix a` opens a popup with every pane of your space, agents first and
whoever needs you on top, with a live preview of the selected pane (the bottom
of its screen, where prompts and dialogs are).

| key | |
| --- | --- |
| `enter` | go to the pane |
| `ctrl-o` | peek: work in that pane right here, inside the popup |
| `ctrl-s` | this space / all spaces |
| `ctrl-a` | agents only / all panes |
| `ctrl-r` | refresh (it also refreshes by itself every 2 s) |
| `esc` | close |

While peeking, the popup is a tmux client of its own on that session: type, run
commands, answer the agent. `prefix a` again (or `prefix d`) closes it. The
peeked window takes the popup's size in the meantime.

## Install, uninstall

```sh
git clone <this repo> ~/.config/tmux
~/.config/tmux/setup.sh
```

`setup.sh` installs the plugins, enables the commit leak guard, creates
`spaces.conf` from `spaces.example` and the sounds folder, builds agentd into
`~/.local/bin/agentd`, and runs `agents/install`, which adds the hook to every
Claude Code profile (`~/.claude`, `~/.config/claude/*/`) and to Codex
(`~/.codex/hooks.json`), keeping a `.pre-agents.bak` of each file.
`agents/install --uninstall` removes it. Needs `jq`, `python3` (for Codex
observation), `fzf`, `uv` (for notifications), a notification daemon (mako),
PipeWire's `pw-play` (for sounds) and a Rust toolchain (`cargo`, for agentd).

### agentd

agentd is the program behind all of the above: one small daemon per tmux
server, started by itself (from the tmux config or the first hook) and gone
with its server. Nothing you see changes with it; it answers the agents'
hooks faster and blinks for a fraction of the CPU. Without `cargo`,
`setup.sh` leaves everything on the bash scripts in `agents/bin`, which do the
same.

- **Is it running?** `~/.local/bin/agentd ctl status` (inside tmux) prints its
  pid, how it talks to tmux (`control`, normally) and what it tracks.
- **After updating the repo:** run `setup.sh` again, then
  `~/.local/bin/agentd ctl stop` in each tmux server and reload the config
  (`prefix R`), which starts the new binary and sets any new hooks. Nothing
  is lost: it saves its state first.
- **Agents started before you installed it** keep calling the bash hook until
  they restart; the bash hook hands their events to agentd.

**Going back to bash,** in this order:

```sh
mkdir -p ~/.local/state/tmux-agents && touch ~/.local/state/tmux-agents/agentd.off
~/.config/tmux/agents/install --bash
tmux set -gu @agentd
~/.config/tmux/agents/bin/agent-spaces load
~/.local/bin/agentd ctl stop
```

`agentd.off` lasts: while it exists, the hooks go to bash (even from agents
that still call agentd), no daemon starts, the tmux config doesn't turn agentd
on, and `setup.sh` registers the bash hook. `agent-spaces load` builds the
top row again for bash (with agentd it reads values the daemon keeps). The last step closes the
notifications agentd had open (a plain `ctl stop`, for an update, keeps them). To return to agentd, delete it,
run `setup.sh` and reload the config (`prefix R`).

## Troubleshooting

- **An agent shows `◇`:** it doesn't report; restart it. For Codex, trust the
  hooks when it asks.
- **An agent stays `●` or `▲` after cancelling:** leave the pane or open mission
  control to re-check its transcript. Codex also observes it automatically; check
  that `python3` is available and the `Interrupt` hook is trusted. Declining one
  tool can let Codex continue its turn.
- **No notifications:** are you looking at that session (then none are sent)?
  Is the space muted (`prefix Q`)? Is mako in privacy mode? Test one by hand:
  `~/.config/tmux/agents/bin/agent-notify "$TMUX_PANE" normal test hello`.
- **No sounds:** is the file there, with the right name? Sounds off or space
  muted (`prefix Q`)? Test: `~/.config/tmux/agents/bin/agent-sound enemy-down`.
- **A state that was wrong (`▲` while nothing waited, say): what did the
  agent send?** `~/.local/state/tmux-agents/events.log` (and `events.log.1`,
  the one before) has a line per event agentd got and per check of its own,
  with the state before and after:
  `grep ' %12 ' ~/.local/state/tmux-agents/events.log | tail -30` (the pane
  id is `#{pane_id}`: `tmux display -p '#{pane_id}'` in that pane). It holds
  event names, modes and tool names only, never your prompts or commands.
- **Anything else:** `touch ~/.local/state/tmux-agents/debug` makes the bash
  hook log every event (with your prompts, in `payloads.log`) and the scripts
  log their errors into that folder; agentd writes its errors to `errors.log` there too, as
  `agentd[<pid>] ...`. Delete the `debug` file when done. `agentd ctl status` shows what agentd tracks; if it
  seems stuck, `agentd ctl stop` (the next hook starts it again).
