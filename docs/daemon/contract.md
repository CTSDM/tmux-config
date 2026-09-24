# agentd: behavior contract

What any implementation of the agent layer must do, observable from outside:
tmux options, sink lines (sounds and notifications), timing. The reference is
the bash implementation at commit **885bb9b** (`agents/bin/*`); where this
text and the reference disagree, ask the architect: one of them has a bug.
Rules marked **CHANGE** are intentional differences from the reference: tests
for them are expected to fail on bash.

Owner: **arquitecto**. Rule ids (`H3`, `S2`...) are stable; tests cite them.

Notation: "now" is one clock read per event (epoch seconds). "Set" writes a
pane option, "unset" removes it. Options are pane options unless stated.

## 1. Inputs

**I1.** An implementation is invoked as a hook with argument `claude` or
`codex` (today `agent-hook <kind>`, later `agentd hook <kind>`), the event as
JSON on stdin, and the environment of the agent (`TMUX`, `TMUX_PANE`,
`CLAUDE_CONFIG_DIR`). The kind is exactly `claude` or `codex`, and both
`TMUX` and `TMUX_PANE` are set; otherwise nothing happens.

**I2.** It never writes to stdout, always exits 0, and returns in well under a
second (Claude's timeout is 10 s, Codex's `Interrupt` 3 s). Whatever goes
wrong (bad JSON, tmux gone, pane gone), the result is "nothing happens".

**I3. Fields used**, each converted to a string, whitespace runs collapsed to
one space, cut to 300 characters (not bytes), missing/null → empty:
`hook_event_name` (ev), `session_id` (sid), `agent_id`, `agent_type`,
`tool_name` (tool), `tool_use_id` (tool id), detail, `notification_type`,
`last_assistant_message` (last), `error`, `permission_mode` (mode), `source`,
`model`, `transcript_path` (transcript); Codex also `turn_id`, `tool_input`
(as a fingerprint, §6) and `tool_response.accepted`.
Detail: if `tool_input` is an object, the value of the first of `command`,
`file_path`, `path`, `pattern`, `url`, `query`, `description` that is present
and neither null nor `false` (jq's `//`: an empty string or `0` counts, and
wins); none → empty. A `tool_input` that is null or `false` gives an empty
detail; any other non-object is the detail itself (as JSON text if not a
string). Tool label: `tool` alone when detail is empty, else `tool: detail`.
How a number is spelled in that JSON text is not part of the contract (jq
keeps the literal `1.50`, serde writes `1.5`).

**I4. Ownership.** Only the pane's own agent counts. Walk from the hook's
parent process up through its ancestors until reaching the pane's
`pane_pid`, looking at no more than 13 processes (the parent and 12 more).
The walk must reach `pane_pid` within them, and exactly one process on it,
`pane_pid` included, must be named (`/proc/<pid>/comm`) `claude` or `codex`.
Otherwise the event is ignored: this drops a `claude -p` or `codex exec` that
an agent runs inside its own pane.
**CHANGE C4.** Bash looks at the names of 12 processes and accepts a
`pane_pid` found 13th without looking at its name: with the 13th being the
only agent it rejects, with another agent among the first 12 it accepts two.
The rule above counts the 13th like the others. Chains of 12 or fewer are
the same in both.

## 2. Pane options (the output)

Everything shown comes from these options; theme.conf, agents.conf, the board,
the session search, `agent-next` and `agent-spaces` read them, so names and
values are part of the contract.

| Option | Value |
|---|---|
| `@agent` | `claude` or `codex` |
| `@agent_state` | `ready`, `working`, `needs`, `compacting`, `done`, `idle`, `error` |
| `@agent_needs` | `permission`, `question` or `plan`; only while `needs` |
| `@agent_needs_id` | id of the call that is waiting; only while `needs` |
| `@agent_since` | epoch seconds of the last state **change** |
| `@agent_prev` | state before compaction |
| `@agent_tool` | last tool label, `#` written as `##` (**CHANGE C9**: every value is stored whole; bash loses a trailing `;`, which tmux reads as a command separator) |
| `@agent_msg` | last reply (done) or error text, `#` written as `##` |
| `@agent_subs` | running subagents; `0` and unset mean the same |
| `@agent_subtypes` | e.g. `2 Explore, 1 Plan`; unset when none |
| `@agent_bg` | background shells or commands still running; unset when none |
| `@agent_session`, `@agent_transcript`, `@agent_mode`, `@agent_model`, `@agent_profile` | identity (H14) |
| `@agent_tests_sound_at` | epoch of the last test-run sound |
| `@agent_turn`, `@agent_outcome`, `@agent_question`, `@agent_collaboration`, `@agent_pid`, `@agent_pid_start` | Codex (§6) |

Session and window options `@blink-*` are in §10. Any other option an
implementation writes (watcher pids, notification ids) is internal and not
part of the contract. **CHANGE C3:** Rust does not write `@agent_bg_watch`,
`@agent_codex_watch`, `@agent_notify_id`, `@agent_notify_pid`.

**P1. Clear all.** "Clear the pane" unsets every option of this table
(`AG_OPTS` in agent-lib.sh) plus internal ones.
**P2. Options can change from outside** (tmux hooks such as `pane-focus-in`,
the user, tests): each event starts from the pane's options as they are at
that moment (`@agent_state`, `@agent_since`, `@agent_prev`, `@agent_needs_id`,
`@agent_tool`, `@agent_tests_sound_at`, `@agent_subs`), never from a copy an
implementation kept. Its own bookkeeping (subagent sets, rounds, Codex calls
and waits, timers) is internal and cannot be seeded this way.

## 3. Claude events → state

Applies to kind `claude`, events without `agent_id` (subagents: §5). "cur" is
the current `@agent_state`.

| Id | Event | State | Also |
|---|---|---|---|
| H1 | SessionStart, `source` ≠ `compact` | `ready` | subagents of the old and new session forgotten (`@agent_subs` → 0, subtypes unset); unset tool, msg |
| H1b | SessionStart, `source` = `compact` | unchanged | |
| H1c | SessionStart (any) | | `@agent_model` = model, if non-empty |
| H2 | UserPromptSubmit | `working` | unset tool, msg; pane joins its space's round (§8) |
| H3 | PreToolUse | `working`, unless cur is `needs` (unchanged) | `@agent_tool` = label |
| H4 | PermissionRequest | `needs` | kind `question` if tool is AskUserQuestion, `plan` if ExitPlanMode, else `permission`; `@agent_needs_id` = tool id; `@agent_tool` = label |
| H5 | PostToolUse, PostToolUseFailure | `working`, except when cur is `needs`, `@agent_needs_id` is non-empty and differs from this tool id (unchanged: another parallel call is still waiting) | |
| H6 | Notification `permission_prompt` | `needs permission`, unless cur is `needs` (unchanged) | |
| H6b | Notification `elicitation_dialog`, `elicitation_url_dialog`, `agent_needs_input` | `needs question` | |
| H6c | Notification, any other type | nothing at all (not even H14) | |
| H7 | Elicitation | `needs question` | |
| H7b | ElicitationResult | `working` if cur is `needs`, else unchanged | |
| H8 | PreCompact | `compacting` | `@agent_prev` = cur, unless cur is already `compacting` |
| H8b | PostCompact | `@agent_prev`, or `idle` if empty or `compacting` | |
| H9 | Stop | `done`, or `idle` when the pane is visible (§7) | `@agent_msg` = last; unset tool; background shells (§9) |
| H10 | StopFailure | `error` | `@agent_msg` = error, else last, else `unknown error` |
| H11 | SessionEnd | | clear the pane (P1), forget its subagents, close its notification |
| H12 | any other event | nothing at all | |

**H13. Writing the state.** When a row gives a state: set `@agent_since` =
now if it differs from cur; set `@agent_state`; if the state is `needs`, set
`@agent_needs` to the kind (when the row gives one); otherwise unset
`@agent_needs` and `@agent_needs_id`.

**H14. Identity**, on every handled event except H6c, H11, H12 and
subagent events: `@agent` = kind; `@agent_session` = sid, `@agent_transcript`
= transcript and `@agent_mode` = mode, each only if non-empty; for Claude,
`@agent_profile` = basename of `CLAUDE_CONFIG_DIR` without a trailing slash
(default `~/.claude`), with `.claude` shown as `default`.

## 4. Order of effects inside one event

**O1.** Options are written first; sounds, notifications and timers after.
A hook that has returned has its options already visible.
**O2.** Events of one pane are applied in the order the hooks ran.
**O3.** Effects of one pane happen in the order of the events that caused
them: a notification an event decides is shown before a later event of that
pane can close it. So a notification never outlives a later close (leaving
`needs`, SessionEnd, seen), and after SessionEnd no option remains.
**CHANGE C5.** Bash starts `agent-notify` detached and it takes ~150 ms (uv)
to publish its id; a close that comes sooner finds nothing to close, and the
notification stays open with an orphan `@agent_notify_id`.

## 5. Subagents

**A1.** An event with a non-empty `agent_id` never changes the main state or
identity. SubagentStart adds that subagent (type = `agent_type`; empty or
`default` count as `agent`), SubagentStop removes it, other events are ignored.
**A2.** After each change: `@agent_subs` = count; `@agent_subtypes` = one
`N type` per type, sorted by type name ignoring case (ties in byte order),
joined by `, ` (unset when 0): `1 agent, 1 Explore, 1 Plan`. Bash gets this
order from `sort` under the user's locale (en_US.UTF-8); under `C` it would
put upper case first.
**A3.** Subagents are per agent session (sid): H1 and H11 forget them.
**A4.** While subagents run, a pane in `done`, `idle` or `ready` shows `◐`
(formats); for the effects, "subagents running" suppresses the done sound and
notification (§8, §11).

## 6. Codex

Codex uses the same events and effects as Claude (§3 rows H1-H3, H8-H9,
H11, H14; `Interrupt` below), but its state comes from per-pane bookkeeping
that survives between hooks. Reference: `agents/bin/agent-codex` at 885bb9b
(`transition()`, `prepare()`, `watch()`, `command_roots()`), and its scenarios
in `agents/tests/test_codex.py`, which the contract suite must cover as black-box
tests.

**X1. Session and turn.** Bookkeeping belongs to one Codex session. An event
of another session is ignored, except SessionStart (not `compact`), which
resets it. UserPromptSubmit with a new `turn_id` starts a turn (forgets calls
and waits). Any other event carrying a `turn_id` different from the current
turn is ignored (stale).

**X2. Calls and waits.** PreToolUse records the call (tool id → fingerprint of
tool name + `tool_input` without its `description` key; SHA-256 of canonical
JSON: sorted keys, no spaces). `request_user_input` also opens a `question`
wait for that call. PermissionRequest has no tool id: it opens a `permission`
wait on the single recorded call with the same fingerprint; if several match,
the wait is "unresolved" and ends when none of those calls is still recorded;
if none match, it stays until the turn ends. PostToolUse closes that call and
its wait.

**X3. State.** SessionStart → `ready` (with `compact`: unchanged); UserPromptSubmit and PreToolUse →
`working`; PostToolUse → `working` unless the turn is over; PreCompact →
`compacting` (remembers the state), PostCompact → remembered state
(`working` if none or `compacting`); Stop → `done` (H9 visibility applies);
`Interrupt` → `idle` (unset tool and `@agent_prev`); SessionEnd → clear (H11).
Stop, Interrupt and SessionEnd end the turn and drop every wait. Then: while
any wait is open, the turn is not over and the state is not `compacting`, the
state is `needs` with the kind of the oldest open wait, and `@agent_needs_id`
is that wait's id.

**X4. Question sent.** PostToolUse of `request_user_input_async` whose
`tool_response.accepted` is true (the response may be a JSON string) sets
`@agent_question` = `sent`; the next UserPromptSubmit unsets it. It survives Stop.

**X5. Observation.** While a Codex turn is open, or commands it started still
run, the implementation re-reads the session's rollout (`transcript_path`,
JSONL) about every 2 s, incrementally (never consuming an unfinished last
line; restarting on truncation or a different file), and applies what no hook
reported, for the current turn only:
- `function_call_output` for a recorded call closes it and its wait;
- `task_complete` with `error` → `error` (as H10, message from
  `error.message`, sound and notification as StopFailure), unless already `error`;
- `turn_aborted` → `idle` (as Interrupt), unless `idle` or `error`;
- `task_complete` → `done` with `last_agent_message` (as Stop), if the state
  is `working`, `needs` or `compacting`;
- otherwise, `needs` with no open wait → `working`.
- The agent process gone (pid + start time no longer match) → clear (H11).

Observation stops when the turn is over (per rollout, or 10 s after a
Stop/Interrupt hook if the rollout never says so) and no command runs, or when
the state is `ready`, or the pane no longer holds that Codex session. A later
hook or reconcile starts it again.

**X6. Codex options**: `@agent_turn` (current turn id), `@agent_outcome`
(`complete`, `interrupted` or `error` once the turn ended, empty while it runs),
`@agent_collaboration` (`collaboration_mode_kind` of the current turn's
`task_started`, e.g. `plan`), `@agent_pid` and `@agent_pid_start` (agent pid
and its start time from `/proc/<pid>/stat`). Empty values are unset.

**X7. Background commands.** `@agent_bg` for Codex counts command trees the
agent started for this session that are still alive: descendants of the agent
process (found by parent pid over all of `/proc`) that lead their own Unix
session (sid = pid, different from the agent's), carry
`CODEX_THREAD_ID=<session id>` in their environment, and are not one of our
helpers nor `codex-code-mode-host`; a tree inside another counts once; a tree
keeps counting while any process of it (pid + start time) lives, even after
reparenting. Recounted at Stop, Interrupt, StopFailure and each observation.

**X8. Privacy.** Codex bookkeeping on disk holds fingerprints, ids and process
identities only: never prompts, commands, tool arguments or output.

## 7. Visibility

**V1.** Computed when an event enters `needs`, or gives `done` or `error`.
The focused client is the tmux client whose terminal window has keyboard
focus: Hyprland's active window pid is the client's process or one of its
ancestors; without Hyprland, the client with tmux's `focused` flag.
`AG_FOCUS_CLIENT` overrides (seams).
**V2.** `visible`: the focused client shows the pane's session, and the pane
is the active pane of the active window. `session`: it shows the session but
not that pane. `away`: anything else, including no focused client.

## 8. Sounds

Sound names are those of `agent-sound`. Every sound below is skipped when the
pane's space is muted (`@agent_mute_<space>` = `on`, the space name with
characters outside `A-Za-z0-9_-` replaced by `_`; the space is the session's
`@space`, else `@space_auto`).

| Id | When | Sound | Visibility |
|---|---|---|---|
| S1 | entering `needs` (cur was not `needs`), kind question / plan / permission | `report-in` / `wait-for-my-go` / `need-backup` | not when `visible` |
| S2 | Stop with no subagents running and the round won (§8 R) | `ct-win` | any |
| S3 | Stop with no subagents running, round not won | `enemy-down` | not when `visible` |
| S4 | StopFailure (and Codex X5 error) | `oh-man` | any |
| S5 | PostToolUse of `ExitPlanMode` (you accepted the plan) | `lets-do-this` | any |
| S6 | PreToolUse of `Bash` whose detail matches the test regex, and at least 600 s since `@agent_tests_sound_at` | `fight-like-a-man`; set `@agent_tests_sound_at` = now (also when muted) | any |
| S7 | reminder (§11) | `come-to-papa` | not when `visible` |

**S8.** Test regex: global `@agent_test_regex`, else the default in agent-hook
(`npm test`, `pytest`, `go test`, `cargo test`, `make check`...: POSIX ERE
with bracket classes). Matched against the collapsed, cut detail.

**S9. Playing** (what reaches the sink): nothing when global `@agent_sound` is
`off`, or no readable file `<name>.{wav,ogg,oga,mp3,flac}` exists in
`$AG_SOUNDS` (default `$XDG_DATA_HOME/tmux-agents/sounds`). Priority:
`need-backup`, `report-in`, `wait-for-my-go`, `oh-man`, `come-to-papa` = 4;
`ct-win` = 3; `enemy-down`, `lets-do-this` = 2; others 1. Within 2.5 s of the
last played sound, a sound plays only with a higher priority, and it stops the
previous one. Volume `@agent_sound_volume` (default 0.8). Across all panes of
one user (one debounce).

**R. Rounds.** A round is per tmux server and space: UserPromptSubmit adds
the pane. At a Stop with no subagents running: if another pane of the same
space is `working`, `needs` or `compacting`, or has subagents, the round goes
on (not won). Otherwise the round ends (forgotten) and is won when it had at
least 2 distinct panes.

## 9. Background shells (Claude)

**B1.** At Stop, count the agent process's children whose command line
contains `shell-snapshots/snapshot-` (shells started by the Bash tool with
`run_in_background`). If any, keep `@agent_bg` = that count up to date, checked
about every 2 s, and unset it when it reaches 0.
**B2.** Reconcile (§12) resumes that for a Claude pane that has such shells
and no one checking.

## 10. Turn signal (blink)

**K1. Targets.** A session or window blinks with kind `needs` when one of its
panes is `needs`. It blinks with kind `unseen` when one of its panes is
`done` with no subagents and no background work, and `@agent_since` is less
than `@agent_unseen_blink_for` seconds ago (global, default 120; `0` = no
limit); `needs` wins. Sessions named `_peek-*` never blink. Demo: global
`@blink-demo` = `<session> <window id> <until epoch>` makes that session and
window blink as `needs` until then, then it is unset.
**K2. Options.** Session: `@blink-s` = `1`, `@blink-s-kind`, `@blink-s-lit`,
`@blink-s-rest`. Window: `@blink-w`, `@blink-w-kind`, `@blink-w-lit`,
`@blink-w-rest`. When a target stops blinking, its four options are unset
within one refresh.
**K3. Text.** Session: its name as `#{=/16/…:session_name}`. Window:
`#{E:@agent-task}` cut to 14 characters with `…` if the session has
`@narrow-tabs`, else 22. Lengths in characters.
**K4. Frames.** 12 per cycle, frame f: lit length = ⌈len·(f+1)/6⌉ for
f < 6, len for f = 6..8, 0 for f = 9..11; lit = first characters, rest = the
others. One frame every 70 ms while any `needs` target exists (unseen targets
then advance every other frame), every 140 ms when only `unseen` ones remain.
**K5. Life.** Targets are re-evaluated every 6 frames. With no targets,
nothing runs (no timer, no process). Starts when a pane enters `needs` or
`done`, when the demo is set and when the configuration loads. One animator
per tmux server.

## 11. Notifications and reminders

**N1. When.** On entering `needs` (cur was not `needs`), on `done`, on
`error`: only when the pane is `away`, its space is not muted, and not
(`done` with subagents running).
**N2. Content.** Title: session name, plus ` · <task>` where task is the pane
title without a leading `✳ ` and without a trailing ` | <anything>` (the last
such part), omitted when it equals the host name. Body and urgency:

| State | Urgency | Body |
|---|---|---|
| needs question | critical | `Has a question for you` |
| needs plan | critical | `Plan ready for your review` |
| needs permission | critical | `Needs permission: <label>` (this event's label for PermissionRequest, else the pane's `@agent_tool` without `##` escaping); `Needs permission` when empty |
| done | normal | last, or `Finished`; plus ` · N shell` / ` · N shells still running` when background work runs |
| error | critical | `Stopped: <error>`, or `Stopped: error` |

**N3. One per pane.** A new notification replaces the pane's previous one.
It closes when the pane is seen (§12 E1), when the pane leaves `needs` for
another state, on SessionEnd, and when its agent is found gone (E2, X5). Clicking it runs `agent-jump <pane>`.
**N4. Reminder.** On entering `needs`, a reminder is armed for
`@agent_remind_after` seconds (global, default 900; read as `sleep` reads
one argument: decimals and an `s`/`m`/`h`/`d` suffix work; empty or
unreadable → 900, **CHANGE C7**: bash's `sleep` fails at once on those and the
reminder fires immediately). When it fires, if the
pane is still in the same `needs` (state `needs`, `@agent_since` unchanged),
its space is not muted and it is not `visible`: sound S7 and a critical
notification, same title, body `Still waiting for you, N min now`
(N = whole minutes since `@agent_since`).
**CHANGE C1.** One reminder per pane: arming replaces the previous one, and
leaving `needs` cancels it (bash leaves one sleeping process per `needs`).
**CHANGE C2.** The reminder compares with the `@agent_since` written by the
same event (bash reads the clock twice and can miss when a second boundary
falls in between). Not deterministic in bash: its test there is a non-strict
expected failure; tests of N4 start their event just after a second boundary
so they don't hit this by chance.

## 12. Seen and reconciliation

**E1. Seen.** When a pane gets focus (tmux `pane-focus-in`): `done` → `idle`,
without changing `@agent_since`; its notification closes.
**E2. Reconcile**, for given panes or all agent panes. Triggers: focus leaves
a pane in `working`, `needs` or `compacting`; mission control opens;
`prefix u`; configuration load.
- The agent process is gone (Claude: `pane_pid` itself if named `claude` or
  `codex`, else a child of it with that exact name) → as H11: clear the pane
  (P1, internal options included) and close its notification.
  **CHANGE C6:** bash's reconcile unsets only the `AG_OPTS` table: internal
  options stay and the notification remains until the pane is seen.
- Claude in `working`, `needs` or `compacting` whose transcript says the turn
  is over → `idle`, `@agent_since` = now, unset needs, needs id, tool. No sound,
  no notification. The turn is over when, among the last 80 lines, the last
  entry of type `user`, `assistant` or `system` with subtype `turn_duration`
  is that system entry, or a user message whose text starts with
  `[Request interrupted by user`. Unreadable transcript: not over.
- Claude background shells: B2.
- Codex: one observation now (X5), and observation resumes if needed.

## 13. Configuration read (global tmux options)

`@agent_sound`, `@agent_sound_volume`, `@agent_remind_after`,
`@agent_test_regex`, `@agent_unseen_blink_for`, `@agent_mute_<space>`,
`@blink-demo`; session options `@space`, `@space_auto`, `@narrow-tabs`.
Changes take effect for the next event or tick (no restart needed).

## 14. Test notes

- Visibility needs a real attached client on the test server (e.g. a pane of
  a second `-L` server running `tmux -L <test> attach`), named in
  `AG_FOCUS_CLIENT`. `AG_FOCUS_CLIENT=none` makes everything `away`.
- Timers: set `@agent_remind_after` and `@agent_unseen_blink_for` to a few
  seconds; the Codex observation tick is ~2 s.
- Sounds need files: point `AG_SOUNDS` at a temp dir with empty `<name>.wav`
  files; with `AG_SINK` nothing plays anyway. `@agent_sound` must be on (the
  default) for sound lines to appear.
- The seams go in the test server's **global** environment (start the server
  with them, or `set-environment -g`), not only in the hook's: tmux hooks such
  as `pane-focus-in` run helpers with the server's environment. The Rust
  daemon reads them from its own environment once, when it starts: fix them
  before any agent or daemon starts, and change focus afterwards only through
  tmux (clients, panes, focus events).
- Bash sink mode keeps a shown notification "open" (a fake id, no waiter)
  until it is replaced or closed, so every close path produces its
  `notify-close` line. A click cannot be simulated; `agent-jump` is out of scope.

## 15. The bar

**U1. Clicking a session.** A left click on a session chip of the top row
(its index, name and glyphs) switches that client to the session, as tmux's
default `MouseDown1Status` does for a `range=session`. The row shows only the
sessions of the client's space, so a click never leaves the space. Clicking a
window tab of the second row selects that window (already so). **CHANGE C8:**
bash's chips have no range: a click on them does nothing.

## 16. The daemon is invisible

**Z1.** An implementation that talks to tmux through a client of its own
(control mode) must not be seen: no window changes size, no bar is laid out
again (`@narrow-tabs`, `@summary-room`, the rows), no pane is marked seen,
`agent-jump` and every client choice keep picking the user's client, no
session of its own shows in the top row, the session search, the board or
`agent-next`, and the user's `session_attached` counts don't change. This
holds when it attaches, while it runs, when its session is killed or the last
user session closes, and when it goes away.

