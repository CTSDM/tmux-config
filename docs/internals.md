# Internals

How the agent-aware tmux setup works, for changing it. For using it, see
[guide.md](guide.md).

## The idea

Agents report, tmux draws. Claude Code and Codex call a hook on their
lifecycle events; the hook writes facts into **pane options** of the agent's
pane (`@agent_state`, `@agent_needs`, ...). The tmux config turns those
options into the bar and the borders with formats: tmux redraws when an option
changes. Observers repair missing Codex events and count background
processes. Everything else (notifications, sounds, spaces, mission control)
reads the same options.

There are two implementations of the hook side, with the same behavior
(`docs/daemon/contract.md`): **agentd**, a Rust daemon per tmux server that
the hooks talk to (`agentd hook`, see [agentd](#agentd)), and the bash scripts
in `agents/bin` (`agent-hook` and its helpers), the reference and the way back.
The rest of this page describes the behavior through the bash scripts, which
spell it out; agentd does the same in one process.

## Files

| file | role |
| --- | --- |
| `tmux.conf` | general options and keys; sets `@agentd` when agentd is installed; sources the two files below, then TPM |
| `theme.conf` | colors (Catppuccin Mocha), status rows, window tabs, borders, menus, popups; the templates of the space line |
| `agents/agents.conf` | agent formats (glyphs, labels, task names), hooks (seen/unseen, re-checks, relayout), keys |
| `agents/install` | registers the hook (`agentd hook` with `--agentd <path>`, `agent-hook` with `--bash`) in every Claude Code profile and in Codex |
| `setup.sh` | installs everything; builds agentd (static and not position independent on glibc: it starts as fast as `/bin/true`) into `~/.local/bin/agentd` (a new file renamed over the old) |
| `agentd/` | agentd, the Rust daemon: `src/core` (states, pure), `src/daemon` (tmux transport, effects, blink, observation), `src/hook.rs` (the hook client); `check.sh` |
| `docs/daemon/` | agentd's design, the behavior contract, its tasks and the control-mode spike |
| `tests/` | the contract suite, run against either implementation |
| `agents/bin/agent-hook` | the hook: event JSON on stdin → pane options, notifications, sounds |
| `agents/bin/agent-codex` | Codex turn/call correlation, incremental rollout reader and execution observer (Python standard library) |
| `agents/bin/agent-lib.sh` | shared helpers: process tree, Hyprland, focused client, per-client formats, visibility |
| `agents/bin/agent-reconcile` | repairs states no hook reported (Esc, denied permission, killed agent) |
| `agents/bin/agent-notify` | desktop notifications over D-Bus (uv script, jeepney) |
| `agents/bin/agent-sound` | plays a sound with priority and debounce |
| `agents/bin/agent-remind` | the 15-minute reminder |
| `agents/bin/agent-blink` | draws the "turn signal" frames on sessions and windows that need you |
| `agents/bin/agent-bgwatch` | counts the shells a Claude agent left running in the background (`@agent_bg`) |
| `agents/bin/agent-spaces` | spaces: tagging, numbering, the space line, narrow layouts, prefix+S / prefix+Q menus, next/prev/go |
| `agents/bin/agent-board` | mission control (fzf) |
| `agents/bin/agent-sessions` | session search (fzf) |
| `agents/bin/agent-new`, `agents/new-session.inputrc` | the new-session form (bash `read -e` with readline Tab completion) |
| `agents/bin/agent-jump`, `agent-next`, `agent-peek` | going to a pane, to the next agent that needs you, peeking |
| `agents/typings/` | type stubs for jeepney (pyright strict) |
| `spaces.conf` | folders → spaces; local, git-ignored |

Runtime state lives outside the repo: `$XDG_RUNTIME_DIR/tmux-agents/` (running
subagents per session, sound debounce, rounds; agentd's socket, lock and state
file per tmux server) and `~/.local/state/tmux-agents/` (debug logs, only with
the `debug` file; `agentd.off`, the way back to bash).

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
| `@agent_notify_id`, `@agent_notify_pid` | agent-notify | the pane's notification and its waiter (bash only; agentd keeps its ids in its state file) |
| `@agent_tests_sound_at` | hook | last "fight like a man" |
| `@agent_bg`, `@agent_bg_watch` | agent-bgwatch | background shells, and the watcher's pid |
| `@agent_turn`, `@agent_outcome` | agent-codex | current Codex turn and its terminal result |
| `@agent_question` | agent-codex | `sent` after an asynchronous question, dismissed by the next user prompt |
| `@agent_collaboration` | agent-codex | collaboration mode from the rollout; independent of `@agent_mode` (permissions) |
| `@agent_pid`, `@agent_pid_start`, `@agent_codex_watch` | agent-codex | root PID, process start time and Codex observer PID |
| `@p-untracked` | agentd | `1` while the pane runs `claude` or `codex` without `@agent` (◇); `@agent-untracked` reads it with agentd, `pane_current_command` without |

## From events to states

| event (Claude / Codex) | state |
| --- | --- |
| SessionStart | `ready` (compaction restarts keep the state) |
| UserPromptSubmit | `working` |
| PreToolUse | `working` (not while waiting for permission) |
| PreToolUse: request_user_input (Codex) | `needs question` until the matching PostToolUse |
| PermissionRequest | `needs`: `question` for AskUserQuestion, `plan` for ExitPlanMode, else `permission` |
| PostToolUse, PostToolUseFailure | `working`; ends `needs` only for the call that asked |
| Notification (Claude) | `needs` for permission prompts and elicitations |
| Elicitation / ElicitationResult (Claude) | `needs question` / `working` |
| PreCompact / PostCompact | `compacting` / back to the previous state |
| Stop | `done`, or `idle` if you are looking at the pane |
| StopFailure (Claude) | `error` |
| Interrupt (Codex) | `idle`, clears the wait and its notification |
| SubagentStart / SubagentStop | subagent count; a subagent's own events never change the main state |
| SessionEnd | all options cleared |

The hook only counts the pane's own agent: it walks up its process tree to the
pane's shell and ignores a `claude -p` or `codex exec` run by another agent.
It never prints and always exits 0 (hook output can land in the conversation;
exit 2 would block a tool call).

**Reconciliation.** Claude can end a turn without a hook; its transcript has
`system/turn_duration` or `[Request interrupted by user`. Codex 0.156.1 has
`Interrupt`, including cancelled approvals; declining a tool can instead let
the model continue. Codex writes `task_started`, `task_complete` (possibly with
`error`) and `turn_aborted`. `agent-reconcile` runs on focus-out, mission control,
`prefix u` and config reload, and clears panes whose agent process is gone.

For Codex it delegates to `agent-codex`. Hooks and observations share a lock per
server socket/pane; session and turn IDs reject old results. Permission requests
lack `tool_use_id`, so the helper matches canonical tool/input fingerprints to
the calls seen by PreToolUse. An ambiguous match stays pending until all matching
calls return; an unmatched request stays pending until the turn ends. Tool output
in the rollout also clears a denied call that never emits PostToolUse. Parallel
results cannot dismiss another call's question or permission wait.

One Codex observer per pane reads appended complete JSONL lines every 2 seconds.
It tracks inode/offset, rebuilds after rotation/truncation, and retains lifecycle
records beyond a fixed tail window. `task_complete.error` publishes `error` using
the same notifications as the shared hook; it does not mark a retry or nonzero
shell exit as a failed turn. The observer exits after terminal state and no live
commands, or when the pane/session disappears. A later hook/reconcile starts it
again. Its cache and locks live under `$XDG_RUNTIME_DIR/tmux-agents/codex-*`;
only input fingerprints, lifecycle metadata and process identities are retained.
The rollout is an internal format; these paths are covered by regression tests.

`request_user_input_async` publishes `question sent` after `accepted=true`; this
is an acknowledgement of sending, not an answer. The badge persists across Stop
and is dismissed by the next user prompt. `plan mode` is separate from a request
to approve a plan; Codex has no ExitPlanMode hook in this version.

**Seen.** `pane-focus-in` turns `done` into `idle` and closes the pane's
notification.

**Background shells.** No hook says when a shell started with
`run_in_background` ends. On `Stop` the hook counts the agent's child
processes started through Claude's shell snapshot (`shell-snapshots/snapshot-`
in the command line, which MCP servers and other children lack); if there are
any, `agent-bgwatch` rechecks every 2 s and keeps `@agent_bg` until they are
gone. `agent-reconcile` starts a missing watcher (after a reload, say). The
formats treat `@agent_bg` like `@agent_subs`: `◐`, counted as working.
Codex's observer also maintains `@agent_bg`: it finds execution roots with the
thread's `CODEX_THREAD_ID` and a new Unix session, then counts each root and its
descendants once (including nested bubblewrap/sandbox processes). PID/starttime
records retain known descendants after reparenting and reject recycled PIDs.
The scan uses PPIDs across all processes, since Rust can spawn from any thread.
Normal MCP children keep the parent's session, so they are excluded. This is a
local heuristic: a custom MCP launched with the same markers and a new session
can resemble a command, and a daemon that detaches before any scan can be missed.
No snapshot filename, shell name or PTY is required. Remote/shared app-server
processes outside the pane's tree require an explicit mapping and are not tracked.

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

## The turn signal

tmux can't animate, and its formats can't cut text at a computed position, so
`agent-blink` draws the frames: every 70 ms it sets, on each session and
window that blinks, the lit and unlit parts of the name as displayed
(`@blink-s-lit`/`@blink-s-rest` on sessions, `@blink-w-*` on windows, plus
`@blink-s`/`@blink-w` as the switch; different names, because a window's
lookup falls back to its session's options). `@blink-s-kind`/`@blink-w-kind`
say which signal it is:

- `needs`: an agent there needs you. theme.conf paints the lit part as an
  amber band.
- `unseen`: an agent finished (`done`, with no subagents or background shells
  left) less than `@agent_unseen_blink_for` seconds ago (default 120, `0` =
  no limit). The lit part gets a dark green background (`@ac-done-soft`)
  under the green text. `needs` wins when a session or window has both.

Each band sits on its element's own background. Changing an option redraws
the bar, which is what moves it. 12 frames: 6 sweeping, 3 lit, 3 dark; 70 ms
each while anything needs you (green bands then advance every other tick),
140 ms when only green ones are left. One animator per
server (a `flock`), started by the hook when a pane enters `needs` or `done`,
and on config load; it rechecks every 6 frames and exits when nothing is left
to blink. Measured cost while it runs, with 6 terminals attached: about 8% of
a core for the tmux server plus ~5% for the animator and its tmux calls; half
that with only green bands; nothing when idle.

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
counts per space (`@cnt-<state>-<space>`, without agentd) and binds `prefix S`
and `prefix Q`.

The status rows are `status-format[N] = #{E:@rowN}`. Globally `@row0` is the
space line (`#{E:@fleet-line}`, per session) and `@row1` the window line
(`@window-line`, tmux's default). The space line is built from three
templates in theme.conf (`@fleet-head-tpl`, `@fleet-chips-tpl`,
`@fleet-tail-tpl`) with placeholders the script fills per space.

`agent-spaces layout` (on resize, attach, a session switch to a session laid
out for another width, a window opened or closed, and after tagging) estimates, for each session shown on a terminal, whether its rows fit the
terminal's width. If not, it sets session-level `@row0..@row4` and `status`:
session chips packed into rows by index range, the summary on the last row or
its own, and window tabs packed by window index (`@narrow-tabs` shortens
titles). The summary chooses words or glyphs by comparing its plain length to
`@summary-room`, the room the layout left on its row, and `@layout-width`
records the width it laid out for.

Moving between sessions (`next`, `prev`, `go`, the search) uses the same
list, sorted by name, that the top row draws with `#{S/n:}`.

**With agentd, the top row counts nothing** (task L1). tmux expands the row
again on every redraw of a client: every blink frame, every option write,
every title change. Without agentd, each chip loops over its session's panes
(`#{W:#{P:#{E:@agent-glyph}}}`) and each count of the summary over every
pane of every session, dozens of loops per redraw, plus a `/proc` read per
pane for `pane_current_command` (the ◇): ~12 ms of the server per redraw per
client at 40 panes. With `@agentd` set, `agent-spaces` fills the templates
with plain values agentd keeps instead (`agentd/src/daemon/bar.rs`): per
session `@s-glyphs`, `@s-unseen` and `@s-other-<state>` (the summary's
counts, unset for none), per pane `@p-untracked`. The row keeps one loop over
the sessions for its chips; ~0.5 ms. agentd reads what they come from in one
`display -p` (50 ms after the news, so a burst makes one read) and writes
only the values that differ, in one request (each write redraws every
client). The news: its own writes that change `@agent`, `@agent_state`,
`@agent_subs` or `@agent_bg` from what the last read found (every event
writes its state again), a window or session added or closed (`%window-*`,
`%sessions-changed` on its control client), a pane closed (the
`after-kill-pane` and `pane-exited` hooks send `agentd bar` to that client,
no process) and sessions tagged by `agent-spaces` (`agentd ctl bar`).
Nothing polls: a control-mode subscription would notice a `claude` typed in
a shell without hooks, but tmux checks one every second and reading every
pane's command cost it ~1.2 ms a second at 40 panes; that ◇ shows at the next
news instead. Without control mode (`AGENTD_TRANSPORT=spawn`) windows and
closed panes wait for the next news too.

## agentd

**Shape.** One binary, four commands: `agentd hook claude|codex` (what the
agents run: reads the event, keeps the fields the contract uses, sends them
with its parent process chain to the daemon, waits for the ack; never prints,
always exits 0), `agentd daemon`, `agentd ensure` (start it unless it runs)
and `agentd ctl status|seen|reconcile|blink|blink-demo|bar|stop`. One daemon per
tmux server: its files are `agentd-<socket name>-<hash>.{sock,lock,state.json}`
in `$XDG_RUNTIME_DIR/tmux-agents/`; the lock makes a second one exit at once,
and a pidfd on the tmux server makes it exit with the server. A hook that
finds no daemon starts one and waits up to 300 ms. Inside, one thread: each
pane has an event queue (its events in order; the ack goes out once its
options are written) and an effect queue (sounds, notifications), and the
state machine (`src/core`) is pure: event and facts in, option writes and
effects out.

**The switch.** `tmux.conf` sets the global option `@agentd` to
`~/.local/bin/agentd` when that file is executable and `agentd.off` doesn't
exist, and unsets it otherwise. Everything else follows it: `agents.conf`
starts the daemon (`ensure`), routes focus to it (a `display-message -l -c`
to its control client, named in `@agentd_client`, which it reads as
`%message agentd seen <pane>`; `ctl seen` when that client is gone), the
reconcile, the blink and prefix+Q's preview; the bash `agent-hook` and
`agent-reconcile` hand over to it (agents started before the switch keep
calling `agent-hook`; old Codex observers end up in `agentd hook` too, where
the process-tree check drops them). `agentd.off` in
`~/.local/state/tmux-agents/` turns it all back: `agentd hook` hands each
event to the bash `agent-hook` of the checkout it was built from (else
`~/.config/tmux/agents/bin/agent-hook`), and no daemon starts.

**Talking to tmux.** A control-mode client of its own (`tmux -u -C`),
attached to its own session `_peek-agentd` (`destroy-unattached`, flags
`no-output,ignore-size`): commands go in as lines, answers come back in
`%begin`/`%end` blocks matched by their tag. It stays out of sight (contract
§16): `_peek-*` sessions are skipped by the bar, the board and the searches,
control-mode clients by every client choice, `prefix s`/`w`/`D` filter both,
a `client-session-changed` hook moves a user's client that lands in
`_peek-agentd` (a plain `tmux attach`) to their most recent session, and when
its session is the last one it closes it and doesn't come back, so the server
exits as it would without it. If the client goes away the daemon spawns one
`tmux` per request until it attaches again; `AGENTD_TRANSPORT=spawn` (in the
daemon's environment) never uses control mode.

**In process.** Sounds (same files and debounce as `agent-sound`), D-Bus
notifications (a click runs `agent-jump`), reminders (timers), the blink (it
takes `agent-blink`'s lock, so the two never draw at once, and writes only
what changes from frame to frame), Codex observation and background shells
(one 2 s tick, only while something is observed), the top row's values (see
[Spaces and the bar](#spaces-and-the-bar)). The state file keeps the
state machine's memory, reminders and notification ids; it belongs to one
tmux server instance (pid and start time) and goes when that server dies.
A pane that goes without its SessionEnd (killed, or its agent crashed) is
forgotten by the sweep, which runs on every full pane list (a Stop, the
observation tick): its reminders at once, its queues once drained, its
state-machine memory (Codex bookkeeping, subagents, round) and notification
at the second list that misses it, since one list may predate a pane or
session just made.

**Debugging.** `agentd ctl status` prints the pid, the transport (`control`
or `spawn`), its state, reminders, queues, observed panes and what blinks.
With `~/.local/state/tmux-agents/debug`, errors go to `errors.log` there as
`agentd[<pid>] ...`. `agentd ctl stop` (or SIGTERM) saves, clears the blink
and exits (the next hook starts the installed binary); with `agentd.off` it
first closes its open notifications, since no daemon will come back to close
them when their panes are seen. To rule out control mode, stop it and
start it by hand from a shell in that tmux server:
`AGENTD_TRANSPORT=spawn setsid -f ~/.local/bin/agentd daemon`.

## Testing

agentd: `agentd/check.sh` (format, clippy, unit tests, integration tests on
isolated tmux servers, `cargo deny`). The contract suite in `tests/` runs the
same black-box tests against either implementation (`tests/README.md`);
`tests/bench/` measures hook latency, memory and CPU.

Run the Codex lifecycle, concurrency, incremental-reader and real tmux/process
regressions without an API connection or changes to user configuration:

```sh
python3 -B agents/tests/test_codex.py -v
```

The suite creates and destroys its own tmux server, with muted alerts. The fake
agent only provides a process identity; the hook, observer and processes are real.

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
- **A big tmux server forks slowly.** Freed memory the allocator keeps makes
  every `fork()` of the server slow (350 ms at 6 GB), and `run-shell` forks
  it. Keys and focus changes use `run-shell -C` (a tmux command, no process)
  and `if -F`; `display-message -c <agentd's client>` is how they reach agentd.
- **The status line is expanded again on every redraw,** and any option
  write redraws every client. A loop over panes in it costs on each blink
  frame, and `pane_current_command` reads `/proc` each time it is expanded;
  what the bar needs of every pane is best kept in a plain option.
- **`display-message` expands `%N` as strftime,** so a pane id in its text
  turns into spaces; `-l` prints it as it is.
- **In `client-session-changed`, `#{client_width}` is some client of the
  session,** not always the one that switched: `#{hook_client}` is, and a
  `#{L:}` loop gets its width.
- **A popup loses its top rows on tmux 3.7c** when the status is at the
  top and the pane under it prints: the pane paints over the popup's first
  rows, one per status line, until the next full redraw.
  `tests/upstream/tmux-3.7c-popup-overlay.patch` fixes it (one line; tmux
  master has no popups any more), with a reproducer next to it.
- **`#{client_name}` in a `#{L:}` loop crashes tmux 3.6** when a client has
  just connected and not identified yet (no name: a NULL `strdup`). Read it
  only behind `#{?client_session,...}`; `list-clients` and `choose-client`
  skip such clients by themselves.
