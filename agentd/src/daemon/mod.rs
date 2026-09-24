//! `agentd daemon`: one per tmux server, until that server is gone.
//!
//! Each pane has an event queue (O2: its events in arrival order; the ack
//! goes out once the options are written, O1) and an effect queue (O3: its
//! effects in the order of the events that caused them). Both go once they
//! are drained and the pane is gone or its agent session ended, so a daemon
//! that runs for weeks doesn't keep every pane it has seen. Everything runs
//! on one thread.

mod effects;
mod procs;
mod rollout;
mod store;
mod tmux;
mod visibility;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use serde_json::json;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{JoinHandle, LocalSet, spawn_local};
use tokio::time::{sleep, timeout};

use crate::core::codex::{self, After, CodexFacts, Procs};
use crate::core::{self, Effect, Event, Facts, Input, Kind};
use crate::identity::{self, Paths};
use crate::procfs;
use crate::proto::{CtlRequest, HookRequest, Reply, Request};
use procs::ProcView;
use store::{Saved, SavedReminder, Server};
use tmux::{Missing, Read, Tmux};
use visibility::Seams;

/// A request line bigger than this is not ours.
const MAX_REQUEST: u64 = 1 << 20;
/// A client that connects and says nothing.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Writes of the state file are grouped.
const SAVE_DELAY: Duration = Duration::from_millis(200);
/// B1's marker of shells started by Claude's Bash tool.
const SNAPSHOT_SHELL: &str = "shell-snapshots/snapshot-";
/// X5: Codex panes are observed about this often.
const TICK: Duration = Duration::from_secs(2);

pub fn run() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&format!("daemon: {e}"));
            ExitCode::FAILURE
        }
    }
}

fn start() -> io::Result<()> {
    let tmux_var = env::var_os("TMUX")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("TMUX is not set"))?;
    let socket = identity::server_socket(&tmux_var)
        .ok_or_else(|| io::Error::other("no tmux socket in TMUX"))?
        .to_path_buf();
    // $TMUX is socket,pid,session: the pid is the server's.
    let server_pid: u32 = tmux_var
        .to_string_lossy()
        .split(',')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| io::Error::other("no server pid in TMUX"))?;
    let dir = identity::runtime_dir(env::var_os("XDG_RUNTIME_DIR").as_deref());
    let paths = Paths::new(&dir, &identity::server_id(&socket));

    // Detach from whoever started us (a hook, tmux.conf, a test).
    let _ = rustix::process::setsid();
    let _ = env::set_current_dir("/");
    fs::create_dir_all(&dir)?;

    // One daemon per server: the lock is held for our whole life.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&paths.lock)?;
    if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        return Ok(());
    }
    // Holding the lock, any socket left there is stale.
    let _ = fs::remove_file(&paths.socket);
    let listener = StdListener::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let pid = Pid::from_raw(server_pid as i32).ok_or_else(|| io::Error::other("bad server pid"))?;
    let server = pidfd_open(pid, PidfdFlags::empty())?;
    // Which instance of the server: a restarted one gets the same socket.
    let instance = Server {
        pid: server_pid,
        start: procfs::stat(server_pid)
            .ok_or_else(|| io::Error::other("tmux server gone"))?
            .starttime,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = LocalSet::new().block_on(
        &runtime,
        serve(paths.clone(), socket, instance, listener, server),
    );
    let _ = fs::remove_file(&paths.socket);
    drop(lock);
    result
}

async fn serve(
    paths: Paths,
    socket: PathBuf,
    instance: Server,
    listener: StdListener,
    server: OwnedFd,
) -> io::Result<()> {
    let listener = UnixListener::from_std(listener)?;
    let server = AsyncFd::new(server)?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;

    let saved = store::load(&paths.state, instance);
    let daemon = Rc::new(Daemon {
        tmux: Tmux::new(socket, instance.pid),
        server: instance,
        seams: Seams::from_env(),
        host: fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_string())
            .unwrap_or_default(),
        state: RefCell::new(saved.core),
        reminders: RefCell::new(HashMap::new()),
        panes: RefCell::new(HashMap::new()),
        watched: RefCell::new(HashMap::new()),
        ticking: Cell::new(false),
        save: Notify::new(),
        paths,
        started_ms: now_ms(),
    });
    for (pane, r) in saved.reminders {
        let left = r.due_ms.saturating_sub(now_ms()) as f64 / 1000.0;
        daemon.arm(&pane, left, r.since);
    }
    spawn_local(daemon.clone().saver());

    let server_gone = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    spawn_local(daemon.clone().connection(stream));
                }
                // Out of descriptors, say: don't spin.
                Err(_) => sleep(Duration::from_millis(50)).await,
            },
            _ = server.readable() => break true,
            _ = term.recv() => break false,
            _ = int.recv() => break false,
        }
    };
    if server_gone {
        // Nothing in it can apply to another server.
        let _ = fs::remove_file(&daemon.paths.state);
    } else {
        daemon.save_now();
    }
    Ok(())
}

/// The effects of one event, with what they need.
type Batch = (effects::Ctx, Vec<Effect>);

/// A batch in a pane's effect queue, and who waits for it to finish.
type Queued = (effects::Ctx, Vec<Effect>, Option<oneshot::Sender<()>>);

enum Job {
    Hook(Box<HookRequest>, oneshot::Sender<Reply>),
    Reminder {
        since: i64,
    },
    /// X5: observe a Codex pane, with this tick's view of /proc.
    Observe(Rc<ProcView>),
    /// E2: reconcile the pane; told when done.
    Reconcile(oneshot::Sender<()>),
}

/// A Codex pane under observation (X5).
#[derive(Debug, Clone)]
struct Watch {
    /// The agent pid to fall back on while the pane names none.
    agent: u32,
    session: String,
    /// When the turn ended by hook without the rollout confirming it.
    finished_since: Option<u64>,
    /// An observation of this pane is queued and not done yet.
    queued: bool,
}

struct Reminder {
    saved: SavedReminder,
    timer: JoinHandle<()>,
}

/// A pane's two queues.
struct PaneQueues {
    events: mpsc::UnboundedSender<Job>,
    effects: mpsc::UnboundedSender<Queued>,
    /// Jobs and effect batches sent and not finished yet.
    pending: Cell<usize>,
    /// The last job found the pane gone, or ended its agent session.
    retire: Cell<bool>,
}

struct Daemon {
    tmux: Tmux,
    server: Server,
    seams: Seams,
    host: String,
    state: RefCell<core::State>,
    reminders: RefCell<HashMap<String, Reminder>>,
    panes: RefCell<HashMap<String, PaneQueues>>,
    watched: RefCell<HashMap<String, Watch>>,
    /// The observation tick runs (only while something is watched).
    ticking: Cell<bool>,
    save: Notify,
    paths: Paths,
    started_ms: u64,
}

impl Daemon {
    async fn connection(self: Rc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        let mut reader = BufReader::new(read.take(MAX_REQUEST));
        let reply = match timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => match serde_json::from_str::<Request>(&line) {
                Ok(Request::Hook(h)) => self.hook(h).await,
                Ok(Request::Ctl(c)) => self.ctl(&c).await,
                Err(e) => Reply::error(format!("bad request: {e}")),
            },
            _ => return,
        };
        if let Ok(mut out) = serde_json::to_vec(&reply) {
            out.push(b'\n');
            let _ = write.write_all(&out).await;
        }
    }

    async fn hook(self: &Rc<Self>, request: Box<HookRequest>) -> Reply {
        let (ack, acked) = oneshot::channel();
        let pane = request.pane.clone();
        self.queue(&pane, Job::Hook(request, ack));
        acked.await.unwrap_or_else(|_| Reply::ok())
    }

    async fn ctl(self: &Rc<Self>, request: &CtlRequest) -> Reply {
        match request.ctl.as_str() {
            "reconcile" => self.reconcile(&request.args).await,
            "status" => Reply::data(json!({
                "pid": std::process::id(),
                "tmux": self.tmux.env(),
                "started_ms": self.started_ms,
                "state": &*self.state.borrow(),
                "reminders": self.reminders.borrow().iter()
                    .map(|(pane, r)| (pane.clone(), r.saved))
                    .collect::<BTreeMap<_, _>>(),
                "panes": self.panes.borrow().keys().cloned().collect::<BTreeSet<_>>(),
                "watching": self.watched.borrow().keys().cloned().collect::<BTreeSet<_>>(),
            })),
            other => Reply::error(format!("unknown command: {other}")),
        }
    }

    /// Runs `f` on the pane's queues, starting them on first use.
    fn with_queues<T>(self: &Rc<Self>, pane: &str, f: impl FnOnce(&PaneQueues) -> T) -> T {
        let mut panes = self.panes.borrow_mut();
        let queues = panes.entry(pane.to_string()).or_insert_with(|| {
            let (events, jobs) = mpsc::unbounded_channel();
            let (effects, batches) = mpsc::unbounded_channel();
            spawn_local(self.clone().pane_events(pane.to_string(), jobs));
            spawn_local(self.clone().pane_effects(pane.to_string(), batches));
            PaneQueues {
                events,
                effects,
                pending: Cell::new(0),
                retire: Cell::new(false),
            }
        });
        f(queues)
    }

    fn queue(self: &Rc<Self>, pane: &str, job: Job) {
        self.with_queues(pane, |q| {
            q.pending.set(q.pending.get() + 1);
            let _ = q.events.send(job);
        });
    }

    async fn pane_events(self: Rc<Self>, pane: String, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            let retire = match job {
                Job::Hook(request, ack) => {
                    let (done, retire) = self.on_hook(&request).await;
                    let _ = ack.send(Reply::ok());
                    if let Some((ctx, effects)) = done {
                        self.run_effects(ctx, effects);
                    }
                    retire
                }
                Job::Reminder { since } => match self.on_reminder(&pane, since).await {
                    Ok(Some((ctx, effects))) => {
                        self.run_effects(ctx, effects);
                        false
                    }
                    Ok(None) => false,
                    Err(missing) => missing == Missing::Pane,
                },
                Job::Observe(view) => match self.observe(&pane, view).await {
                    Ok(Some((ctx, effects))) => {
                        self.run_effects(ctx, effects);
                        false
                    }
                    Ok(None) => false,
                    Err(missing) => missing == Missing::Pane,
                },
                Job::Reconcile(done) => {
                    let retire = match self.on_reconcile(&pane).await {
                        Ok(Some(((ctx, effects), cleared))) => {
                            self.run_effects(ctx, effects);
                            cleared
                        }
                        Ok(None) => false,
                        Err(missing) => missing == Missing::Pane,
                    };
                    let _ = done.send(());
                    retire
                }
            };
            self.finished(&pane, Some(retire));
        }
    }

    async fn pane_effects(
        self: Rc<Self>,
        pane: String,
        mut batches: mpsc::UnboundedReceiver<Queued>,
    ) {
        while let Some((ctx, effects, done)) = batches.recv().await {
            for effect in &effects {
                effects::run(&ctx, effect).await;
            }
            if let Some(done) = done {
                let _ = done.send(());
            }
            self.finished(&pane, None);
        }
    }

    /// A job (with its verdict on the pane) or an effect batch is done; the
    /// pane's queues go if nothing is pending and the pane is retired.
    fn finished(&self, pane: &str, retire: Option<bool>) {
        let mut panes = self.panes.borrow_mut();
        let Some(q) = panes.get(pane) else { return };
        q.pending.set(q.pending.get().saturating_sub(1));
        if let Some(retire) = retire {
            q.retire.set(retire);
        }
        if q.retire.get() && q.pending.get() == 0 {
            // Dropping the senders ends both queues' tasks.
            panes.remove(pane);
            drop(panes);
            self.cancel(pane);
            self.watched.borrow_mut().remove(pane);
        }
    }

    /// Panes that no longer exist (from a full pane list): their queues go
    /// once drained, and their reminders now.
    fn sweep(&self, existing: &HashSet<&str>) {
        let gone: Vec<String> = self
            .panes
            .borrow()
            .keys()
            .filter(|p| !existing.contains(p.as_str()))
            .cloned()
            .collect();
        for pane in gone {
            let idle = {
                let panes = self.panes.borrow();
                panes.get(&pane).is_some_and(|q| {
                    q.retire.set(true);
                    q.pending.get() == 0
                })
            };
            if idle {
                self.panes.borrow_mut().remove(&pane);
            }
        }
        let orphans: Vec<String> = self
            .reminders
            .borrow()
            .keys()
            .filter(|p| !existing.contains(p.as_str()))
            .cloned()
            .collect();
        for pane in orphans {
            self.cancel(&pane);
        }
        self.watched
            .borrow_mut()
            .retain(|p, _| existing.contains(p.as_str()));
    }

    /// One hook event, and whether its pane's queues can go (the pane is
    /// gone, or its agent session ended).
    async fn on_hook(self: &Rc<Self>, request: &HookRequest) -> (Option<Batch>, bool) {
        match self.handle_hook(request).await {
            Ok(Some(batch)) => {
                let e = &request.event;
                (Some(batch), e.ev == "SessionEnd" && e.agent_id.is_empty())
            }
            Ok(None) => (None, false),
            Err(missing) => (None, missing == Missing::Pane),
        }
    }

    /// Read, ownership, core, write: everything before the ack.
    async fn handle_hook(self: &Rc<Self>, request: &HookRequest) -> Result<Option<Batch>, Missing> {
        let (kind, ev) = (request.kind, request.event.ev.as_str());
        let want_vis = core::may_need_visibility(kind, ev);
        let want_panes = core::may_need_panes(kind, ev);
        let read = self.tmux.read(&request.pane, want_vis, want_panes).await?;
        if want_panes {
            self.sweep(&read.others.iter().map(|o| o.pane.as_str()).collect());
        }
        let chain = request
            .chain
            .iter()
            .map(|(pid, comm, _)| (*pid, comm.as_str()));
        let Some(agent_pid) = core::owner(chain, read.pane_pid) else {
            return Ok(None); // I4
        };
        let vis = if want_vis {
            Some(self.visibility(&read).await)
        } else {
            None
        };
        let bg_shells = if core::may_need_bg_shells(kind, ev) {
            procfs::count_children_matching(agent_pid, SNAPSHOT_SHELL)
        } else {
            0
        };
        let codex = (kind == Kind::Codex).then(|| {
            // The agent's start time, from the chain the hook walked.
            let start = request
                .chain
                .iter()
                .find(|(pid, _, _)| *pid == agent_pid)
                .map(|(_, _, s)| *s);
            let view = core::may_need_procs(kind, ev).then(|| Rc::new(ProcView::scan()));
            self.codex_facts(&request.pane, &request.event, &read, start, view)
        });
        let facts = self.facts(&read, vis, bg_shells, codex);
        let input = Input {
            kind,
            pane: request.pane.clone(),
            event: request.event.clone(),
            config_dir: request.env.get("CLAUDE_CONFIG_DIR").cloned(),
            agent_pid,
            observing: false,
        };
        let (batch, _) = self.run_core(&input, &facts, &read).await;
        Ok(Some(batch))
    }

    /// The core on one event, then everything up to the ack: the close that
    /// must come first, the writes, the timers, observation. Returns the
    /// effects left for the effect queue, and the writes made.
    async fn run_core(
        self: &Rc<Self>,
        input: &Input,
        facts: &Facts,
        read: &Read,
    ) -> (Batch, Vec<core::Op>) {
        let mut effects = {
            let mut state = self.state.borrow_mut();
            let before = state.clone();
            let effects = core::handle(&mut state, input, facts);
            if *state != before {
                self.save.notify_one();
            }
            effects
        };
        let ctx = self.ctx(&input.pane, read);
        // Phase 1: the bash notifier keeps its id in the pane. As in bash, the
        // close comes before the writes, so SessionEnd leaves no option behind;
        // through the effect queue, after the effects of earlier events (O3).
        let close = effects.iter().position(|e| *e == Effect::NotifyClose);
        if let Some(i) = close.filter(|_| ctx.notify_open) {
            effects.remove(i);
            let (done, finished) = oneshot::channel();
            self.queue_effects(ctx.clone(), vec![Effect::NotifyClose], Some(done));
            let _ = finished.await;
        }
        let ops = match effects.first() {
            Some(Effect::Options(ops)) => ops.clone(),
            _ => Vec::new(),
        };
        if let Some(i) = effects.iter().position(|e| *e == Effect::Watch) {
            effects.remove(i);
            self.watch(&input.pane, input.agent_pid, &input.event.sid);
        }
        ((ctx, self.apply(&input.pane, effects).await), ops)
    }

    /// Codex's facts: the rollout read on from the pane's bookkeeping.
    fn codex_facts(
        &self,
        pane: &str,
        event: &Event,
        read: &Read,
        agent_start: Option<u64>,
        view: Option<Rc<ProcView>>,
    ) -> CodexFacts {
        let base = codex::rollout_base(&self.state.borrow(), pane, &read.pane);
        CodexFacts {
            rollout: rollout::read_on(codex::rollout_path(event, &read.pane), base),
            agent_start,
            procs: view.map(|v| v as Rc<dyn Procs>),
            helpers: read.bin.clone(),
        }
    }

    /// X5: start observing a Codex pane unless it is already.
    fn watch(self: &Rc<Self>, pane: &str, agent: u32, session: &str) {
        self.watched
            .borrow_mut()
            .entry(pane.to_string())
            .or_insert_with(|| Watch {
                agent,
                session: session.to_string(),
                finished_since: None,
                queued: false,
            });
        if !self.ticking.replace(true) {
            spawn_local(self.clone().ticker());
        }
    }

    /// One tick for all watched panes, with one scan of /proc; it stops when
    /// nothing is watched.
    async fn ticker(self: Rc<Self>) {
        loop {
            sleep(TICK).await;
            let due: Vec<String> = {
                let mut watched = self.watched.borrow_mut();
                if watched.is_empty() {
                    self.ticking.set(false);
                    return;
                }
                watched
                    .iter_mut()
                    .filter(|(_, w)| !w.queued)
                    .map(|(pane, w)| {
                        w.queued = true;
                        pane.clone()
                    })
                    .collect()
            };
            if due.is_empty() {
                continue;
            }
            let view = Rc::new(ProcView::scan());
            for pane in due {
                self.queue(&pane, Job::Observe(view.clone()));
            }
        }
    }

    /// X5, one tick: observe, then whether to go on observing (`watch()` of
    /// agent-codex).
    async fn observe(
        self: &Rc<Self>,
        pane: &str,
        view: Rc<ProcView>,
    ) -> Result<Option<Batch>, Missing> {
        let Some(mut w) = self.watched.borrow_mut().get_mut(pane).map(|w| {
            w.queued = false;
            w.clone()
        }) else {
            return Ok(None);
        };
        let read = match self.tmux.read(pane, true, true).await {
            Ok(read) => read,
            Err(missing) => {
                if missing == Missing::Pane {
                    self.watched.borrow_mut().remove(pane);
                }
                return Err(missing);
            }
        };
        self.sweep(&read.others.iter().map(|o| o.pane.as_str()).collect());
        let Some((batch, after)) = self.observe_now(pane, &read, view, w.agent).await else {
            self.watched.borrow_mut().remove(pane);
            return Ok(None);
        };
        let keep = {
            let state = self.state.borrow();
            codex::keep_watching(
                &mut w.session,
                &after,
                state.codex.get(pane),
                now_ms(),
                &mut w.finished_since,
            )
        };
        let mut watched = self.watched.borrow_mut();
        if keep {
            if let Some(entry) = watched.get_mut(pane) {
                entry.session = w.session;
                entry.finished_since = w.finished_since;
                entry.agent = after.agent_pid.parse().unwrap_or(w.agent);
            }
        } else {
            watched.remove(pane);
        }
        Ok(Some(batch))
    }

    /// X5: what the rollout and /proc say that no hook did, now. `None` when
    /// the pane holds no Codex session.
    async fn observe_now(
        self: &Rc<Self>,
        pane: &str,
        read: &Read,
        view: Rc<ProcView>,
        fallback: u32,
    ) -> Option<(Batch, After)> {
        if read.pane.sid.is_empty() || read.pane.agent != "codex" {
            return None;
        }
        let agent = read.pane.agent_pid.parse().unwrap_or(fallback);
        let start = view.start_of(agent);
        // The agent is gone when its pid and start time no longer match.
        let gone = match start {
            None => true,
            Some(s) => {
                !read.pane.agent_pid_start.is_empty() && read.pane.agent_pid_start != s.to_string()
            }
        };
        let event = Event {
            ev: if gone { "SessionEnd" } else { "CodexReconcile" }.into(),
            sid: read.pane.sid.clone(),
            turn: read.pane.turn.clone(),
            transcript: read.pane.transcript.clone(),
            ..Event::default()
        };
        let codex = self.codex_facts(pane, &event, read, start, Some(view));
        let vis = Some(self.visibility(read).await);
        let facts = self.facts(read, vis, 0, Some(codex));
        let input = Input {
            kind: Kind::Codex,
            pane: pane.to_string(),
            event,
            config_dir: None,
            agent_pid: agent,
            observing: true,
        };
        let (batch, ops) = self.run_core(&input, &facts, read).await;
        Some((batch, After::from(&read.pane, &ops)))
    }

    /// E2 for the given panes, or every agent pane; done when all are.
    async fn reconcile(self: &Rc<Self>, panes: &[String]) -> Reply {
        let panes: Vec<String> = if panes.is_empty() {
            let fmt = "#{pane_id}\x1f#{@agent}";
            match self
                .tmux
                .run(&["list-panes".into(), "-a".into(), "-F".into(), fmt.into()])
                .await
            {
                Ok(out) => out
                    .lines()
                    .filter_map(|l| l.split_once('\x1f'))
                    .filter(|(_, agent)| !agent.is_empty())
                    .map(|(pane, _)| pane.to_string())
                    .collect(),
                Err(_) => return Reply::error("tmux did not answer"),
            }
        } else {
            panes.to_vec()
        };
        let mut done = Vec::new();
        for pane in panes {
            let (tx, rx) = oneshot::channel();
            self.queue(&pane, Job::Reconcile(tx));
            done.push(rx);
        }
        for rx in done {
            let _ = rx.await;
        }
        Reply::ok()
    }

    /// E2 for one pane: the batch, and whether the pane was cleared.
    async fn on_reconcile(self: &Rc<Self>, pane: &str) -> Result<Option<(Batch, bool)>, Missing> {
        let read = self.tmux.read(pane, true, true).await?;
        let p = &read.pane;
        if p.agent.is_empty() {
            return Ok(None);
        }
        let apid = procfs::agent_of(read.pane_pid);
        if p.agent == "codex" {
            // One observation now, and observation again if it still runs.
            let view = Rc::new(ProcView::scan());
            let Some((batch, after)) = self.observe_now(pane, &read, view, apid.unwrap_or(0)).await
            else {
                return Ok(None);
            };
            if let (false, Ok(tracked)) = (after.session.is_empty(), after.agent_pid.parse()) {
                self.watch(pane, tracked, &after.session);
            }
            return Ok(Some((batch, after.session.is_empty())));
        }
        let ctx = self.ctx(pane, &read);
        let Some(apid) = apid else {
            // The agent is gone: as H11, notification and internal options too (C6).
            let input = Input {
                kind: Kind::Claude,
                pane: pane.to_string(),
                event: Event {
                    ev: "SessionEnd".into(),
                    sid: p.sid.clone(),
                    ..Event::default()
                },
                config_dir: None,
                agent_pid: 0,
                observing: false,
            };
            let facts = self.facts(&read, None, 0, None);
            let (batch, _) = self.run_core(&input, &facts, &read).await;
            return Ok(Some((batch, true)));
        };
        if core::reconcile::busy(&p.state) {
            let lines = procfs::tail_lines(&p.transcript, core::reconcile::TRANSCRIPT_LINES);
            if lines.is_some_and(|l| core::reconcile::turn_over(l.iter().map(String::as_str))) {
                let now = (now_ms() / 1000) as i64;
                self.tmux.write(pane, &core::reconcile::idle(now)).await;
            }
        }
        // B2: background shells nobody watches.
        let watcher = procfs::entries(p.bg_watch.parse().unwrap_or(0), "cmdline")
            .is_some_and(|argv| argv.iter().any(|a| a.contains("agent-bgwatch")));
        let mut effects = Vec::new();
        if !watcher && procfs::count_children_matching(apid, SNAPSHOT_SHELL) > 0 {
            effects.push(Effect::Bgwatch { agent_pid: apid });
        }
        Ok(Some(((ctx, effects), false)))
    }

    async fn on_reminder(
        self: &Rc<Self>,
        pane: &str,
        since: i64,
    ) -> Result<Option<Batch>, Missing> {
        // Replaced or cancelled after it was queued: not ours any more.
        {
            let mut reminders = self.reminders.borrow_mut();
            if reminders.get(pane).map(|r| r.saved.since) != Some(since) {
                return Ok(None);
            }
            reminders.remove(pane);
        }
        self.save.notify_one();
        let read = self.tmux.read(pane, true, false).await?;
        let vis = self.visibility(&read).await;
        let effects = core::reminder(since, &self.facts(&read, Some(vis), 0, None));
        Ok(Some((self.ctx(pane, &read), effects)))
    }

    fn facts(
        &self,
        read: &Read,
        vis: Option<core::Visibility>,
        bg_shells: u32,
        codex: Option<CodexFacts>,
    ) -> Facts {
        Facts {
            codex,
            now: (now_ms() / 1000) as i64,
            host: self.host.clone(),
            pane: read.pane.clone(),
            vis,
            others: read.others.clone(),
            bg_shells,
            remind_after: read.remind_after.clone(),
            test_regex: read.test_regex.clone(),
        }
    }

    fn ctx(&self, pane: &str, read: &Read) -> effects::Ctx {
        effects::Ctx {
            pane: pane.to_string(),
            bin: read.bin.clone(),
            notify_open: !read.notify_id.is_empty(),
            tmux_env: self.tmux.env().to_string(),
        }
    }

    async fn visibility(&self, read: &Read) -> core::Visibility {
        let focused = visibility::focused_client(&self.seams, &read.clients).await;
        visibility::visibility(focused, read)
    }

    /// Writes the options (O1) and handles the timers; returns the effects
    /// that remain for the effect queue.
    async fn apply(self: &Rc<Self>, pane: &str, effects: Vec<Effect>) -> Vec<Effect> {
        let mut rest = Vec::new();
        for effect in effects {
            match effect {
                Effect::Options(ops) => {
                    self.tmux.write(pane, &ops).await;
                }
                Effect::RemindArm { after, since } => self.arm(pane, after, since),
                Effect::RemindCancel => self.cancel(pane),
                other => rest.push(other),
            }
        }
        rest
    }

    fn run_effects(self: &Rc<Self>, ctx: effects::Ctx, effects: Vec<Effect>) {
        if !effects.is_empty() {
            self.queue_effects(ctx, effects, None);
        }
    }

    /// Into the pane's effect queue; `done` is told when the batch has run.
    fn queue_effects(
        self: &Rc<Self>,
        ctx: effects::Ctx,
        effects: Vec<Effect>,
        done: Option<oneshot::Sender<()>>,
    ) {
        let pane = ctx.pane.clone();
        self.with_queues(&pane, |q| {
            q.pending.set(q.pending.get() + 1);
            let _ = q.effects.send((ctx, effects, done));
        });
    }

    /// C1: one reminder per pane; arming replaces the previous one.
    fn arm(self: &Rc<Self>, pane: &str, after: f64, since: i64) {
        self.cancel(pane);
        let wait = Duration::try_from_secs_f64(after).unwrap_or(Duration::ZERO);
        let daemon = self.clone();
        let target = pane.to_string();
        let timer = spawn_local(async move {
            sleep(wait).await;
            daemon.queue(&target, Job::Reminder { since });
        });
        let saved = SavedReminder {
            due_ms: now_ms() + wait.as_millis() as u64,
            since,
        };
        self.reminders
            .borrow_mut()
            .insert(pane.to_string(), Reminder { saved, timer });
        self.save.notify_one();
    }

    fn cancel(&self, pane: &str) {
        if let Some(r) = self.reminders.borrow_mut().remove(pane) {
            r.timer.abort();
            self.save.notify_one();
        }
    }

    async fn saver(self: Rc<Self>) {
        loop {
            self.save.notified().await;
            sleep(SAVE_DELAY).await;
            self.save_now();
        }
    }

    fn save_now(&self) {
        let saved = Saved {
            v: 1,
            server: Some(self.server),
            core: self.state.borrow().clone(),
            reminders: self
                .reminders
                .borrow()
                .iter()
                .map(|(pane, r)| (pane.clone(), r.saved))
                .collect(),
        };
        if let Err(e) = store::save(&self.paths.state, &saved) {
            log(&format!("saving state: {e}"));
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Like the bash scripts: errors go to a log only while
/// `$XDG_STATE_HOME/tmux-agents/debug` exists.
fn log(message: &str) {
    let state = env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
    let Some(dir) = state.map(|s| s.join("tmux-agents")) else {
        return;
    };
    if !dir.join("debug").exists() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("errors.log"))
    {
        let _ = writeln!(f, "agentd[{}] {message}", std::process::id());
    }
}
