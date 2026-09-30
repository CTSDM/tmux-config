# agentd: design

A resident Rust process that replaces the hot path of the bash agent layer:
`agent-hook`, `agent-codex`, `agent-remind`, `agent-bgwatch`, `agent-blink`,
`agent-sound`, `agent-notify` and `agent-reconcile`. The popups (board,
sessions, new, peek, jump, next) and `agent-spaces` stay in bash for now.

Owner of this file: **arquitecto**. Others propose changes by message.
Behavior to preserve is in [contract.md](contract.md), work in [tasks.md](tasks.md).

## Why

Measured on 2026-09-24 against the bash version (commit 885bb9b):

| What | Today | Target |
|---|---|---|
| Claude hook, per event | ~25 ms median (Stop ~180 ms) | p50 ≤ 5 ms, p99 ≤ 15 ms |
| Codex hook, per event | ~25 ms + 3 Python starts | same as Claude |
| Resident helpers | one bash + `sleep` (~11 MB) per reminder, never cancelled; one Python per pending notification (~15 MB + uv); one bash per watched pane | one process, RSS ≤ 10 MB |
| Blink animator | ~5% of a core (a `tmux` process per frame) | ≤ 1% (tmux's own redraw, ~8%, is not ours) |
| Idle | 0 | 0: no timers run when nothing blinks or is watched |

The wins are fewer moving pieces and one place for timers, not raw speed.

## Shape

One binary, `agentd`, with subcommands:

- `agentd hook claude|codex`: the hook, called by Claude Code and Codex
  exactly like `agent-hook` today (same stdin JSON, same environment). It
  parses the payload, keeps only the fields the contract uses (truncated as
  the contract says; Codex tool input only as a fingerprint), collects its
  parent process chain, sends one message to the daemon and waits for the
  ack. Never prints, always exits 0. If the daemon is not running it starts
  it (`agentd ensure`) and retries for up to 300 ms, then gives up silently
  (reconciliation repairs a lost event).
- `agentd daemon`: one per tmux server. Runs until the tmux server is gone.
- `agentd ensure`: starts the daemon for the current tmux server unless it
  runs; idempotent (flock). `tmux.conf` calls it on load.
- `agentd bridge`: on the desktop, for tmux servers on other hosts: shows
  and plays the notifications and sounds their daemons send over an
  ssh-forwarded socket, and sends clicks back (`daemon/bridge.rs`,
  guide.md "On a remote server").
- `agentd remote <host> <name>` and `agentd hold [<name>]`: a pane whose
  shell runs on another host (see "Remote panes").
- `agentd ctl <command> [args]`: small requests for tmux hooks, key bindings
  and scripts, e.g. `seen <pane>` (pane-focus-in), `reconcile [pane...]`,
  `blink-demo <session> <window> [seconds]`, `status` (JSON dump for
  debugging and tests), `reload` (re-read tmux options).

### Daemon identity

The tmux server is `${TMUX%%,*}` (its socket path). Runtime files live in
`$XDG_RUNTIME_DIR/tmux-agents/` (same folder as bash):
`agentd-<id>.sock`, `agentd-<id>.lock`, `agentd-<id>.state.json`, where
`<id>` is the socket's basename plus the first 8 hex digits of the SHA-256 of
the full path (e.g. `default-3f2a9c1b`).

### Hook → daemon protocol

Unix stream socket, one connection per hook call, one JSON line each way:

```
→ {"v":1,"kind":"claude","pane":"%12","event":{...fields...},
   "chain":[[pid,"comm",starttime],...],"env":{"CLAUDE_CONFIG_DIR":"..."},
   "t":1790201509123}
← {"ok":true}
```

The daemon acks after the pane options are written (so a hook that returned
is already visible, and events from one pane stay in order), before any
sound, notification or timer runs. The ownership check (contract §2) runs in
the daemon with `chain`, since the daemon knows each pane's `pane_pid`.
A parked session's hook (contract I5) adds
`"parked":{"agent":pid,"viewer":[[pid,"comm",starttime],...]}`: the pane and
server come from the viewer, and the ownership check runs on its chain.

### Inside the daemon

- **Core, pure:** `fn handle(&mut State, Input, &Ctx) -> Vec<Effect>`. No I/O,
  no clock (time comes in `Ctx`). Inputs: hook events, `ctl` requests, timer
  ticks, process/rollout observations. Effects: option writes, sounds,
  notifications, timer (re)arming, spawning `agent-jump`. Unit tests live
  here and cover most of the contract without tmux.
- **tmux transport** behind a trait. Decided after the spike
  ([spike-control-mode.md](spike-control-mode.md)):
  - Phases 1-3 spawn `tmux`, one batch per event (read) and one per write.
  - Phase 4 moves to control mode with the spike's recipe (case k): the
    daemon's own session `_peek-agentd` (`destroy-unattached on`), flags
    `no-output,ignore-size`, size `1000x100`, reattach after `%exit` while
    the server lives. Always `tmux -u`: a daemon started without a UTF-8
    locale would otherwise get a client tmux treats as ASCII, and every
    control character (our separators) and non-ASCII character (`✳`, `…`)
    would come back as `_`. `AGENTD_TRANSPORT=spawn` turns control mode off. The session name alone is not enough protection: every
    client consumer also skips `client_control_mode` clients (Rust visibility,
    `agent-spaces layout`, `agent-jump`, `ag_focused_client`). Output is not
    escaped: match `%end`/`%error` by command number, and never read
    program-controlled strings through it without that. Spawning can't reach
    the blink target (the cost is per call, ~5% at 14 frames/s); control mode
    can.
  - Subscriptions (`refresh-client -B` with `S:`/`W:`/`P:` loops) are for
    phase 4 too: panes gone, sessions renamed, spaces changed, without
    polling. Hook-time reads stay (a subscription can be a second stale).
- **Lifetime:** `$TMUX` holds the server's pid (`socket,pid,session`); the
  daemon watches it with a pidfd and exits when it ends. No polling.
- **Visibility:** asks Hyprland's request socket (`.socket.sock`,
  `j/activewindow`) in-process instead of running `hyprctl`; same stale
  signature fallback as `ag_hyprctl`. `AG_FOCUS_CLIENT` overrides (seams).
- **Notifications:** one D-Bus session connection (zbus), signal match set
  once; notification id ↔ pane map in memory; a click runs `agent-jump`.
- **Timers:** reminder (one per pane, re-armed or cancelled on every state
  change), blink frames, background-shell polling, Codex observation and
  permission waits (contract H5b) (one shared 2 s tick that scans `/proc`
  once for every watched pane; a permission wait reads its agent's children).
- **State file:** small JSON snapshot (subagents, rounds, Codex bookkeeping,
  reminder due times, background groups) written on change, debounced, and
  read on start. Pane options stay the source of truth for anything shown;
  on start the daemon rebuilds from them and reconciles.
- **Runtime:** tokio current-thread. Dependencies need the architect's OK
  (see Rules). Starting set: `serde`, `serde_json`, `tokio`, `zbus`
  (`default-features = false`, tokio), `sha2`, `regex`, `libc` or `rustix`.

### Migration (strangler)

Until phase 3 the daemon runs effects by spawning the existing bash helpers
(`agent-sound`, `agent-notify`, `agent-remind`, `agent-bgwatch`,
`agent-blink`), so phase 1 is already a complete replacement of the hook and
the contract suite can pass end to end. Each later phase moves one family of
effects in-process; the daemon stops using that bash script, but the script
stays: it is the bash implementation, and the way back. The live system
switches only at cutover (phase 5), with the user. The bash implementation is
deleted only after the user has run agentd live for a while (phase 7).

### The switch: `@agentd`

One global tmux option says which implementation runs: `@agentd` = the path
of the `agentd` binary, or unset for bash. The same `agents.conf` and bash
entry points follow it, so tests, cutover and rollback use one config:

- `agents.conf` on load: `agentd ensure` when `@agentd` is set.
- `agent-reconcile` (called by agents.conf, the board, `prefix u`) hands over
  to `agentd ctl reconcile` when `@agentd` is set. Only once the daemon
  serves `ctl reconcile` itself (T2.4): before that, each would call the other.
- Later phases route the rest the same way: seen (`pane-focus-in`) in
  phase 3, the blink and its demo in phase 4.
- The agents' hooks are the exception: `agents/install` writes either
  `agent-hook` or `agentd hook` into the Claude and Codex settings (phase 5),
  since a hook that asked tmux first would lose the latency we gained.

Rollback: see the end of the next paragraph.

**Agents already running at cutover.** Claude Code and Codex read their hook
commands when they start: agents running when `agents/install` switches to
`agentd hook` keep calling the bash `agent-hook` until restarted. Two owners
for their panes (the bash Codex bookkeeping next to the daemon's observer and
reconcile) and two animators (bash `agent-blink` and the daemon's blink) must
not happen, so `agent-hook` itself follows the switch: with `@agentd` set it
hands the event to `agentd hook` (`exec`, one extra tmux call, only for the
agents started before cutover). From the moment `@agentd` is set every pane
has one owner, the daemon. Phase 5 designs the reverse (rollback) the same
way, with one persistent file, `${XDG_STATE_HOME:-~/.local/state}/tmux-agents/agentd.off`
(on disk: a rollback survives a reboot). While it exists, `agentd hook` gives
its events to the bash `agent-hook` (one `stat` per event), the daemon
doesn't start, `tmux.conf` leaves `@agentd` unset and `setup.sh` installs the
bash hooks. Rollback, in order: create that file, `agents/install --bash`,
`tmux set -gu @agentd` on each server, `agentd ctl stop`. Back to agentd:
delete it, run `setup.sh`, reload tmux.

## Remote panes (issue #4, option B)

Sessions on a server reached over ssh, shown as local sessions: one tmux (the
local one), no second bar or prefix. The server runs no tmux, only `agentd`
(one executable, #2), which keeps each remote shell alive as dtach would.

```
local tmux pane                               server
agentd remote <host> <name>  ──ssh -T──▶  agentd hold <name>   (a byte pipe)
  │ raw tty ⇄ frames                            │ unix socket
  │                                           holder <name> ── pty ── $SHELL -l
  ▼ events, as the pane's own                    ▲                    └ claude
local agentd daemon                             └── agentd hook claude (AGENTD_HOLD)
```

- **`agentd remote <host> <name>`** runs in a local pane (the pane's
  command, or typed in its shell). It marks the pane `@agent_remote
  <host>:<name>`, runs `ssh -T <host> agentd hold <name>` and speaks frames
  over its stdio: keys and window sizes up, output and hook events down. It
  puts the terminal in raw mode only once the holder answers, so ssh can ask
  for a password or a host key first. Host `-` runs `agentd hold` here,
  without ssh (tests, trying it out).
- **`agentd hold <name>`** on the server connects its stdio to the holder of
  `<name>`, starting it if there is none. **The holder** (`hold --serve`,
  its own session, a lock per name) owns a pty and the program in it (the
  login shell, started on the first attach with that client's `TERM` and
  size), keeps its last 1 MiB of output, and serves one client at a time: a
  new one detaches the old (the pane moved). Its files are
  `${TMUX_TMPDIR:-/tmp}/agentd-<uid>/hold-<name>.{sock,lock}`, not the
  runtime folder: logind removes that at logout (no linger), and the held
  shell must outlive the ssh. `agentd hold` with no name lists them.
- **Hooks on the server:** the held program has `AGENTD_HOLD` (the holder's
  socket) instead of `TMUX`/`TMUX_PANE`. `agentd hook` sends it the event
  with its parent chain; the holder checks I4 against its own child (the
  held shell plays `pane_pid`), acks, and passes the event down, or keeps it
  (up to 1000) while no client is attached.
- **In the local daemon** an event from `agentd remote` is the pane's own:
  I6 (the chain of `agentd remote` reaches `pane_pid` with no agent on it)
  replaces I4, and the agent pid is 0, so everything that reads the agent's
  processes (B1, B2, H5b) or files (E2's transcript, Codex's rollout) sees
  nothing. The rest (states, glyphs, blink, borders, visibility, reminders,
  notifications, sounds) is the local path unchanged. Reconcile leaves a pane
  with `@agent_remote` alone; `agentd remote` unsets it and reconciles the
  pane when it exits.
- **Reconnecting.** The client counts the output bytes it has shown. When
  ssh drops without the holder's exit frame it says so in the pane and
  tries again (1 s, doubling, up to 30 s; ctrl-c gives up, the shell stays
  held). On attach it sends that count, and the holder replays what the
  pane missed, if it still has it; a new pane (count absent) gets the whole
  buffer from its first full line, so a TUI's screen comes back. Events
  kept while nobody was attached go down before the output.
- **Ends:** the held program exits → the holder sends its status, the
  client exits with it (the pane closes). Killing the local pane only
  detaches: the program keeps running on the server.
- **Frames:** one type byte, a 4-byte big-endian length, the payload.
  Up: `H` hello (JSON: `term`, `rows`, `cols`, `have`), `I` input, `R`
  resize (JSON). Down: `A` attached (JSON: `new`), `O` output, `E` event
  (the hook's request, plus `agent`, the pid I4 found), `X` exit (JSON:
  `code`), `D` detached (JSON: `why`). The hook's connection to the holder
  is one `K` frame (its request) and one `K` back (the reply); the listing's,
  one `Q` and one `Q` back (`attached`, `running`).

Not yet: Codex on a remote pane (its facts are the rollout file and /proc,
on the server: events are dropped), B1/B2/H5b there (the holder could count
the shells and send them), parked sessions (I5) there, peek and the
transcript-based reconcile, rebuilding the local sessions after a local
reboot from `agentd hold`'s list, and a remote pane in the `prefix N` form.

## Test seams (both implementations)

Needed so the same black-box suite runs against bash and Rust without
touching the desktop. Bash gets them in T0.2; Rust honors the same variables.

| Variable | Effect |
|---|---|
| `AG_SINK=<file>` | Sounds and notifications are appended to `<file>` as JSON lines instead of played/shown (see below). No D-Bus, no player. |
| `AG_FOCUS_CLIENT=<client name>` or `none` | The focused tmux client, instead of asking Hyprland (`none`: no client has focus, so every pane is `away`). |
| `XDG_RUNTIME_DIR` | Already moves all runtime files; tests always point it at a temp dir. |
| `AG_SOUNDS`, `AG_SOUND_PLAYER` | Already exist in `agent-sound`. |

Sink lines, one JSON object each, `t` in epoch milliseconds:

```
{"t":…,"effect":"sound","name":"need-backup"}
{"t":…,"effect":"notify","pane":"%3","urgency":"critical","title":"api · Fix CSV","body":"Needs permission: Bash: ls"}
{"t":…,"effect":"notify-close","pane":"%3"}
```

`sound` is written only for a sound that would really play: after the
`@agent_sound` switch, the missing-file check and the 2.5 s priority
debounce. `notify` and `notify-close` are written where D-Bus would be called.

## Phases

0. **Preparation:** this design, the contract, bash test seams, contract
   suite green on bash, crate skeleton, control-mode spike, baseline numbers.
1. **Core, hook, daemon:** every Claude event and state of the contract,
   subagents, ownership, rounds; effects via the bash helpers. Suite green.
2. **Codex:** port `agent-codex` (fingerprints, rollout reader, command roots,
   observer). Suite green, including the scenarios of `agents/tests/test_codex.py`.
3. **Effects in-process:** sounds, D-Bus notifications, background shells,
   seen (reminders and reconcile are already in).
4. **Blink in-process**, over control mode if T0.3 says so.
5. **Cutover:** `agents/install` points hooks at `agentd hook`, `tmux.conf`
   and `agents.conf` call `agentd`, docs updated, rollback tested. With the user.
6. **Optional:** `agent-spaces` counts and layout.
7. **Cleanup,** after a while live: delete the bash implementation.

## Roles and branches

All three sessions run in the tmux session `tmux-ricing` of the **live**
server. Worktrees of `~/.config/tmux` under `~/repos/github.com/ctsdm/tmux-ricing/`:

| Session | Worktree | Branch | Owns |
|---|---|---|---|
| arquitecto | `review/` | `daemon` (integration) | `docs/daemon/*`, reviews, merges, measurements |
| implementador | `impl/` | `daemon-impl` | `agentd/`, bash test seams in `agents/`, Rust unit tests |
| tester | `tests/` | `daemon-tests` | `tests/` (contract suite, fixtures, benchmarks) |

Flow: work on your branch, commit, push, then message **arquitecto** with the
task id and commit. The architect reviews, runs the suite, merges into
`daemon` (`--no-ff`), pushes, updates tasks.md and tells you when to
`git merge daemon`. Read the docs from `review/docs/daemon/` (always current),
don't edit them: send the proposal. Questions about behavior go to the
architect; the answer ends up in the contract.

The tester writes tests from the contract, not from the Rust code. When the
contract is unclear, ask; don't infer it from either implementation.

## Rules

1. **The live system is off limits.** `~/.config/tmux` (the `main` checkout the
   live hooks run from) is read-only. Never run `agents/install`, never edit
   `~/.claude*`, `~/.config/claude/*` or `~/.codex/*`.
2. **Never talk to the live tmux server.** Your shell has `TMUX` set to it.
   Every test runs `env -u TMUX -u TMUX_PANE` against `tmux -L <unique name>`
   and kills that server by name when done. Never attach clients to the live
   server, never `tmux kill-server` without `-L`.
3. **No real sounds or notifications.** Tests always set `AG_SINK` and
   `XDG_RUNTIME_DIR` (temp dir) in the test server's global environment. Second
   line of defense, so a missed sink still stays silent: `AG_SOUND_PLAYER`
   pointing at a no-op, and `DBUS_SESSION_BUS_ADDRESS` unset (the fallback bus
   under the temp `XDG_RUNTIME_DIR` does not exist). Don't use `@agent_sound
   off` or a muted space for this: they would hide what the tests check;
   use them only in the tests of those switches.
4. **Kill by pid or by `-L` name**, never `pkill -f <pattern>` (it can match
   your own shell).
5. **Only fake agents.** A test agent is a copy of the Python interpreter
   named `claude` or `codex`; no real Claude or Codex runs (they cost tokens)
   unless the user asks.
6. **No private data in commits.** Fixtures are synthetic. Never copy
   `~/.local/state/tmux-agents/payloads.log` (bash's debug log: it holds
   prompts; before 2026-09-25 it was `events.log`, now agentd's structural
   log). The leak guard (gitleaks + private words, on the changes and on the
   message) must pass; never bypass it.
7. **Dependencies:** Rust crates only from crates.io, each new one approved by
   the architect, `Cargo.lock` committed, `cargo deny check` clean. Python via
   `uv` (PEP 723 or a `pyproject.toml` in `tests/`) and `pyright` strict.
8. **Checks before handing over:** Rust: `cargo fmt --check`, `cargo clippy
   --all-targets -- -D warnings`, `cargo test`. Tests: `pyright` clean, suite
   run on bash (and Rust when it exists) with the result in the message.
9. **Style:** match the repo: short comments that say why, English in code
   and docs.
