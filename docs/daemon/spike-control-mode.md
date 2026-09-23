# Spike: tmux control mode (T0.3)

tmux 3.6, 2026-09-24, by implementador. Driver: a stdlib Python script
(control client over pipes, hooks logged with `set -gaF`), kept out of the repo.
Decision: [design.md, "tmux transport"](design.md).

## Setup

- **Server A** (`-L` unique name) loads the worktree's full `tmux.conf`:
  theme.conf, agents.conf, and TPM + tmux-sensible from the live plugins folder
  (read only). Effective: `window-size latest`, `aggressive-resize on`
  (sensible), `focus-events on`, `exit-empty on`, `detach-on-destroy on`.
- **Isolation in A's global environment:** temp `XDG_RUNTIME_DIR` and
  `XDG_STATE_HOME`, `AG_SINK`, `AG_SOUND_PLAYER` no-op, `AG_FOCUS_CLIENT=none`,
  `AG_SPACES_FILE` (`lab <dir>`, `home *`), no `DBUS_SESSION_BUS_ADDRESS` or
  `HYPRLAND_INSTANCE_SIGNATURE`, `SHELL=/bin/sh`. Panes run `sleep`.
- **Sessions:** `main` (4 windows with long names, space lab), `work` (lab, one
  pane prints every 0.2 s), `api` (home).
- **A user client** of 160x40 on `main`, running in a pane of a second `-L`
  server.
- **Hooks logged** at index 90: client-attached, client-detached,
  client-session-changed, client-resized, client-active, client-focus-in/out,
  pane-focus-in/out, window-resized, session-window-changed,
  window-pane-changed, session-created/closed/renamed, window-layout-changed.
- **Four runs, same results.** Each time the sink stayed empty, nothing under
  `~/.config/tmux` was written (TPM and sensible only ran `tmux` commands
  against A), and every test server was killed by name.

## Verdict

1. **Control mode writes are about 3.5x faster per call** and remove the spawned
   `tmux` processes: about 5% of a core at 14 frames/s. The tmux server's own
   cost is the same either way.
2. **A control client is not invisible.** Attached to an ordinary session, it:
   - makes agent-spaces re-lay out the user's bar for 80 columns, and the bar
     stays that way after the control client leaves;
   - becomes agent-jump's "most recently active client";
   - carries tmux's `focused` flag;
   - fires `pane-focus-in` on the session's active pane, which marks a `done`
     pane as seen (confirmed: `done` became `idle`). No attach flag prevents
     this, `active-pane` included.
3. **One setup had no visible effect in these tests (case k):** the control
   client makes its own session named `_peek-agentd`. Every consumer in
   `agents/` and theme.conf already skips `_peek-*`. The setup also needs
   `destroy-unattached on`, `no-output,ignore-size` and a wide size
   (`refresh-client -C 1000x100`). A control client cannot live without a
   session, so when its session dies it must reattach. `no-detach-on-destroy`
   is not a substitute: moving to another session fires `pane-focus-in` there.
4. **Subscriptions (`refresh-client -B`) can watch the whole server.** Using
   `S:`/`W:`/`P:`/`L:` loops, they report changes anywhere within about 0.5 s
   (at most once a second), at negligible cost. `%*` only covers the attached
   session.

## Q1. Side effects of attaching

Each case: attach, wait 2 s, 30 `display -p` commands, wait 2 s, close stdin,
wait 2 s. Snapshots before and after:

- per session: `session_attached`, `session_many_attached`, `@summary-room`,
  `@narrow-tabs`, `status`, `@row0-4`;
- per window: size and `window_active_clients`.

| Case | Hooks fired by the attach | Effect | On detach |
|---|---|---|---|
| a. `-f no-output,ignore-size -t main` (the user's session) | client-session-changed, client-attached, window-layout-changed, window-resized | **agent-spaces lays out `main` for 80 columns:** `@narrow-tabs 1`, `@summary-room` 135→55, status 2→3 rows, `@row0-2` set. Window main:1 goes 160x38→160x37 (from the extra status row). `session_attached` 1→2, `session_many_attached` 0→1, `window_active_clients` 1→2 | client-detached, client-active (user). **The narrow layout stays**: nothing re-runs layout on client-detached |
| b. same flags, `-t work` (nobody shows it) | as a, plus **pane-focus-in** on work's active pane | `work` laid out for 80 columns, `main` untouched | client-detached |
| c. as a, then `refresh-client -C 1000x100` as the next command | as a | no layout change, but only because the async layout ran after the resize: a race | client-active, client-detached |
| g. as a, with `\; refresh-client -C 1000x100` on the attach command line | as a | no layout change | as c |
| d. `-f no-output` (no ignore-size), no size set | as a | same as a. A control client without `refresh-client -C` has no height (`80x`) and does not size windows | as a |
| i. `-f no-output`, then `refresh-client -C 1000x100` | as a | **main:1 160x38 → 1000x100**: the user's window grows past the terminal (latest client wins). Once a size is set, `ignore-size` is mandatory | windows resized back |
| e. `-f ignore-size` (with output) on `work` | as a | 20 `%output` lines in 2 s from the one printing pane: `no-output` is needed | |
| j. fresh sessions whose active pane is `@agent_state done`: `no-output,ignore-size`, with and without `active-pane` | pane-focus-in, client-session-changed, client-attached | **`done` → `idle` in both** (agents.conf `pane-focus-in[40]`; `[41]` would close its notification) | |
| h. own session: `-C new-session -A -s _agentd`, then `refresh-client -f no-output,ignore-size` | session-created (agent-spaces assign gives it a space from its folder), pane-focus-in (its own pane), client-session-changed | `main` untouched, but `_agentd` shows in every session list and is still agent-jump's pick | **session left behind** |
| k. own session: `-C new-session -A -s _peek-agentd <cmd> \; set -t =_peek-agentd: destroy-unattached on \; refresh-client -f no-output,ignore-size \; refresh-client -C 1000x100` | session-created, pane-focus-in (its own pane), client-session-changed, window-layout-changed/window-resized (no size changed) | **none**: main's layout, sizes and fleet line unchanged; agent-jump's pick stays the user | client-detached, session-closed: **session gone** |

- Commands sent through the control client fired no hooks and did not change
  its `client_activity`. The attach itself sets it, so the control client is
  the newest client from the start.
- `client-attached` does not fire when the control client creates its own
  session (cases h and k), only `client-session-changed`.
- `new-session` also accepts `-f flags`, which would set them from the first
  moment. Not tested; case k sets them in the next command and saw no effect.

## Q2. Which session, and what happens to it

- **It must be attached to a session.**
  - `attach` with no sessions: `%error` "no sessions", `%exit`, exit 1.
  - Unknown target: `%error` "can't find session: …", exit 1.
- **Renamed:** `%session-renamed $id name`; it stays attached.
- **Killed, default flags:** `%sessions-changed`, `%exit`; the process exits 0
  (client-detached hook).
- **Killed, with `no-detach-on-destroy`:** it moves to another session
  (`%session-changed`). That fires client-session-changed and **pane-focus-in
  on the new session's active pane** (the seen problem again).
- **Last session killed:** `%exit` even with `no-detach-on-destroy`.
  - With `exit-empty on` (this config) the server exits.
  - With it off the server stays, but nothing can attach until a session exists.
- **Server gone** (kill-server or last session): `%exit`, then EOF on stdout
  within 3-4 ms. This gives the daemon an immediate "tmux server gone" signal.
- **Closing stdin** detaches cleanly (exit 0).

So the daemon needs its own session (case k). It recreates the session and
reattaches whenever it gets `%exit` while the server is still up.

## Q3. How it shows in `list-clients`

| Format | Value (flags `no-output,ignore-size`) |
|---|---|
| `client_name` | `client-<pid of the tmux -C process>` |
| `client_control_mode` | `1` |
| `client_flags` | `attached,focused,control-mode,ignore-size,no-output,UTF-8` |
| `client_width` x `client_height` | `80x` (no height) until `refresh-client -C`, then e.g. `1000x` |
| `client_termname` | `$TERM` of the process that started it |
| `client_activity` | attach time; commands don't update it |

Consumers that see it:

| Consumer | With the control client on an ordinary session | With case k (`_peek-agentd`) |
|---|---|---|
| agents.conf `client-attached`, `client-session-changed`, `client-resized` → `agent-spaces layout` | takes the narrowest client per session, so the user's session is laid out for 80 columns (a); stays after detach | unaffected: layout skips `_peek-*` sessions |
| agents.conf `pane-focus-in[40]`/`[41]` | an attach or a move marks the session's active `done` pane seen and closes its notification (j). `hook_client` is empty in that hook, so it cannot filter by client | only its own pane gets focus: no agent there |
| `agent-jump` | "viewer" = first client on the pane's session (`ag_raise_client` fails on the control client); fallback = most recently active client, i.e. the control client, so `switch-client -c` would move it instead of the user | unaffected: fallback filters `_peek-*` sessions |
| `ag_focused_client` (agent-lib.sh), without Hyprland | the control client has the `focused` flag; the first focused client in the list wins | still seen: its session never matches, so a pane the user sees could come out `away` if the control client is listed first |
| session lists: `agent-spaces` (space_list, numbering, next/prev/go), `agent-board`, `agent-sessions`, `agent-next`, `agent-blink`, theme fleet chips | a dedicated session named otherwise (h) shows in all of them, and assign gives it a space | all skip `_peek-*` |
| tmux's own choose-tree / choose-client (prefix s, w, D) | shows it | still shows it |

Whatever the setup, the Rust visibility code (contract V1) must skip clients
with `client_control_mode` itself. `agent-spaces layout` and `agent-jump`
could also skip them; the change is cheap and doesn't depend on the session
name.

## Q4. Latency and CPU

The driver is Python on the same machine. Control mode goes through a
`tmux -C` child process that relays to the server.

Per call, n = 300:

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| `set -p`, spawn `tmux` | 4.6-4.8 ms | 5.4-5.7 | 6.2-7.2 | 8.4 |
| `set -p`, control mode | 1.2-1.4 ms | 1.4-1.7 | 1.7-2.1 | 2.6 |
| blink frame (6 `set`), spawn | 5.4-6.1 ms | 6.2-7.2 | 6.9-8.7 | 9.1 |
| blink frame (6 `set`), control mode | 1.6-1.7 ms | 1.9-2.0 | 2.1-2.4 | 2.8 |

Python adds fork/exec overhead to the spawn numbers and a thread hand-off to
the control numbers; the ratio is what carries over.

CPU at 14 blink frames/s (6 sets each, so 84 option writes/s), over 30 s, in %
of one core. The last column is the user client and the second server that
hosts it:

| | tmux server | spawned `tmux` | `tmux -C` relay | user client / host |
|---|---|---|---|---|
| idle | 0.10-0.15 | 0 | 0 | 0 / 0 |
| spawn, targets on the shown session | 3.5-3.6 | 4.8-5.4 | — | 0 / 0.23-0.27 |
| control, same | 3.5-3.6 | 0 | < 0.03 (below tick resolution) | 0 / 0.23 |
| spawn, targets on a session nobody shows | 2.6 | 4.9-5.0 | — | 0 / 0.20 |
| control, same | 2.6-2.65 | 0 | < 0.03 | 0 / 0.25 |

- Control mode saves the spawned processes, about 5% at this rate.
- The server's 2.6% is command processing (84 sets/s), the same both ways and
  even when no client shows the targets. About 0.9 point more is redraw when
  the targets are on screen.
- Derived, not measured: 14 single `set -p` per second would cost the server
  about 0.4%, while spawning would still cost about 5% (the cost is per call).

## Q5. Subscriptions (`refresh-client -B name:what:format`)

**Timing.** A notification arrives about 500 ms after a change (1 s timer), at
most once a second per subscription. It carries the whole value.

**Scope.**
- `%*`, `@*` or `%N` cover only the control client's attached session: a change
  in another session gives nothing. With a dedicated session that's useless.
- A session-scoped subscription (`what` empty) whose format loops covers the
  whole server. Tested with 127 panes in 23 sessions:

| Format | Reports | Tested |
|---|---|---|
| `#{S:#{W:#{P:#{pane_id}=#{@agent_state} }}}` | any pane's option; panes created or closed anywhere | change in another session ✓, pane killed in another session ✓ |
| `#{S:#{session_id}=#{session_name}/#{@space}/#{@space_auto} }` | sessions created, renamed, closed; space changes | rename of another session ✓ |
| `#{L:#{client_name}=#{client_session} }\|#{S:#{session_name}=#{window_id}.#{pane_id} }` | which session each client shows, and each session's active window and pane (the inputs of V2) | window change ✓, active pane change in another session ✓, client switching session ✓ |

**`L:` pitfall.** Inside `L:`, `window_id` and `pane_id` are not the client's:
a subscription with `L:` alone missed the user changing window. Combine `L:`
with `S:`.

**Cost.** With 127 panes, 3 such subscriptions and nothing changing, the server
went from 0.45-0.55% to 0.50-0.60% (noise level, 0.1 point at most); the
control client stayed at 0.

**Use.** Subscriptions tell the daemon about panes gone, sessions renamed,
spaces changed and clients switching session, without polling. They don't
replace the read at hook time: V1 visibility for an event should still be read
then, since a subscription can be a second stale. Formats must leave out options
the daemon writes itself; otherwise every write echoes back within a second.

**Plain notifications.** `%sessions-changed` arrives for every session created
or destroyed anywhere. `%session-renamed` and `%window-*` come only for the
attached session; other sessions give `%unlinked-window-*`.

## Protocol notes for an implementation

- **One block per command:** `a ; b ; c` on one line answers with three
  `%begin`/`%end` blocks.
- **Block header:** `%begin <time> <number> <flags>`. Flags are 1 for our
  commands and 0 for the initial attach or new-session block.
- **Output is not escaped.** An option set to `%end 1790204838 283 1` prints
  that exact line inside the block. Match `%end`/`%error` to its `%begin` by
  number. Don't read strings that programs control (pane titles, task names)
  through the control client unless that check is in place.
- **`display -p` expands strftime `%` sequences** (`%e` became the day of the
  month).
- **Errors:** `%error` blocks carry the message, e.g. `invalid option: @nope`.
- **Notifications** never appear inside a block.

## Options

- **A. Spawn `tmux` per batch (phases 1-3 as planned).** No side effects.
- **B. Control mode (phase 4) with the recipe of case k.** Details:
  - Its session's pane runs something inert; `destroy-unattached on` removes
    the session when the daemon exits.
  - Reattach after `%exit` while the server lives; EOF means the server is
    gone.
  - Still visible: tmux's choose-tree/choose-client, and the no-Hyprland
    fallback of `ag_focused_client`.
  - The `_peek-` prefix reuses the existing filters. Another name (e.g.
    `_agentd`) would need a filter in agent-spaces (space_list, layout,
    assign), agent-board, agent-sessions, agent-next, agent-jump, agent-blink
    and theme.conf's fleet chips.
- **C. Subscriptions** only come with B, since they need the control client.

The implementer's read: B works but its safety rests on a naming convention
and on every future consumer skipping `_peek-*` or control clients. The saving
is the spawned processes (about 5% while something blinks). Reducing the number
of frame writes in the Rust blink would help both options. That is a phase 4
design question and was not measured here.
