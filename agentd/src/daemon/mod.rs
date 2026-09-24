//! `agentd daemon`: one per tmux server, until that server is gone.
//!
//! Each pane has an event queue (O2: its events in arrival order; the ack
//! goes out once the options are written, O1) and an effect queue (O3: its
//! effects in the order of the events that caused them). Both go once they
//! are drained and the pane is gone or its agent session ended, so a daemon
//! that runs for weeks doesn't keep every pane it has seen. Everything runs
//! on one thread.

mod effects;
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

use crate::core::{self, Effect, Facts, Input};
use crate::identity::{self, Paths};
use crate::procfs;
use crate::proto::{CtlRequest, HookRequest, Reply, Request};
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
    Reminder { since: i64 },
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
                Ok(Request::Ctl(c)) => self.ctl(&c),
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

    fn ctl(&self, request: &CtlRequest) -> Reply {
        match request.ctl.as_str() {
            "status" => Reply::data(json!({
                "pid": std::process::id(),
                "tmux": self.tmux.env(),
                "started_ms": self.started_ms,
                "state": &*self.state.borrow(),
                "reminders": self.reminders.borrow().iter()
                    .map(|(pane, r)| (pane.clone(), r.saved))
                    .collect::<BTreeMap<_, _>>(),
                "panes": self.panes.borrow().keys().cloned().collect::<BTreeSet<_>>(),
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
        let facts = self.facts(&read, vis, bg_shells);
        let input = Input {
            kind,
            pane: request.pane.clone(),
            event: request.event.clone(),
            config_dir: request.env.get("CLAUDE_CONFIG_DIR").cloned(),
            agent_pid,
        };
        let effects = {
            let mut state = self.state.borrow_mut();
            let before = state.clone();
            let effects = core::handle(&mut state, &input, &facts);
            if *state != before {
                self.save.notify_one();
            }
            effects
        };
        let ctx = self.ctx(&request.pane, &read);
        let mut effects = effects;
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
        Ok(Some((ctx, self.apply(&request.pane, effects).await)))
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
        let effects = core::reminder(since, &self.facts(&read, Some(vis), 0));
        Ok(Some((self.ctx(pane, &read), effects)))
    }

    fn facts(&self, read: &Read, vis: Option<core::Visibility>, bg_shells: u32) -> Facts {
        Facts {
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
