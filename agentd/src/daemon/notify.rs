//! N1-N3 over D-Bus, as agents/bin/agent-notify does, in-process: one
//! session connection, one match for the notification signals, one open
//! notification per pane (its id in memory and in the state file, never in
//! pane options: C3). A click runs `agent-jump <pane>`. With a desktop
//! bridge (bridge.rs) they go over it instead, and its clicks come back.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::poll_fn;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use zbus::export::futures_core::Stream;
use zbus::proxy::{Builder, CacheProperties};
use zbus::zvariant::Value;
use zbus::{Connection, Proxy};

use super::bridge::{Link, OnClick};
use super::sound::append_line;
use crate::core::Urgency;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP: &str = "tmux agents";
/// No session bus yet (a tmux started before the desktop): try again, but
/// not more often than this.
const RETRY: Duration = Duration::from_secs(30);

pub struct Notifier {
    /// `AG_SINK` (tests): lines instead of D-Bus, which is never touched.
    sink: Option<PathBuf>,
    /// The session bus's address; `None` is the usual one.
    address: Option<String>,
    bus: Mutex<Option<Proxy<'static>>>,
    /// When connecting last failed, and how long until trying again.
    failed: Cell<Option<Instant>>,
    retry: Duration,
    attempts: Cell<u32>,
    /// The open notification of each pane.
    ids: Rc<RefCell<BTreeMap<String, u32>>>,
    /// Sink mode's ids (a notification server's own counter).
    next_fake: RefCell<u32>,
    /// `@agents_bin` and `$TMUX`, for agent-jump on a click.
    jump: Rc<RefCell<(String, String)>>,
    /// The server shows markup in the body (`body-markup`).
    markup: Cell<bool>,
    /// Instead of agent-jump (the desktop bridge's clicks go back).
    on_click: Option<OnClick>,
    /// The desktop bridge, tried before D-Bus.
    link: Option<Rc<Link>>,
    save: Rc<Notify>,
}

impl Notifier {
    pub fn new(
        sink: Option<PathBuf>,
        ids: BTreeMap<String, u32>,
        save: Rc<Notify>,
        link: Option<Rc<Link>>,
    ) -> Notifier {
        let ids = Rc::new(RefCell::new(ids));
        let jump = Rc::new(RefCell::new((String::new(), String::new())));
        if let Some(link) = &link {
            let (ids, jump, save) = (ids.clone(), jump.clone(), save.clone());
            link.on_click(Rc::new(move |pane: &str| {
                if ids.borrow_mut().remove(pane).is_some() {
                    save.notify_one();
                    let (bin, tmux) = jump.borrow().clone();
                    clicked(&bin, &tmux, pane);
                }
            }));
        }
        Notifier {
            sink,
            address: None,
            bus: Mutex::new(None),
            failed: Cell::new(None),
            retry: RETRY,
            attempts: Cell::new(0),
            ids,
            next_fake: RefCell::new(1),
            jump,
            markup: Cell::new(false),
            on_click: None,
            link,
            save,
        }
    }

    /// Clicks go to `f` (with the pane) instead of agent-jump.
    pub fn on_click(&mut self, f: OnClick) {
        self.on_click = Some(f);
    }

    /// A made-up id, as a notification server keeps the one it replaces.
    fn fake_id(&self, previous: Option<u32>) -> u32 {
        previous.unwrap_or_else(|| {
            let mut next = self.next_fake.borrow_mut();
            *next += 1;
            *next
        })
    }

    /// For the state file.
    pub fn ids(&self) -> BTreeMap<String, u32> {
        self.ids.borrow().clone()
    }

    /// Shows a notification for `pane`, replacing its open one.
    pub async fn show(
        &self,
        pane: &str,
        urgency: Urgency,
        title: &str,
        body: &str,
        bin: &str,
        tmux: &str,
    ) {
        *self.jump.borrow_mut() = (bin.to_string(), tmux.to_string());
        self.notify(pane, urgency, title, body, None).await;
    }

    /// As `show`, for a pane on another host (the desktop bridge). Where it
    /// comes from takes the title (`シ SSH · user@host`), what it says the
    /// body; the host is in the app name too, which a notification server
    /// can style (mako: `[app-name="tmux agents · <host>"]`).
    pub async fn show_remote(
        &self,
        origin: Origin<'_>,
        key: &str,
        urgency: Urgency,
        title: &str,
        body: &str,
    ) {
        self.notify(key, urgency, title, body, Some(origin)).await;
    }

    async fn notify(
        &self,
        pane: &str,
        urgency: Urgency,
        title: &str,
        body: &str,
        origin: Option<Origin<'_>>,
    ) {
        let previous = self.ids.borrow().get(pane).copied();
        let id = match &self.sink {
            Some(sink) => {
                let line = serde_json::json!({
                    "t": now_ms(), "effect": "notify", "pane": pane,
                    "urgency": urgency.as_str(), "title": title, "body": body,
                });
                append_line(sink, &line.to_string());
                self.fake_id(previous)
            }
            None if self
                .bridged(json!({"notify": {
                    "pane": pane, "urgency": urgency.as_str(), "title": title, "body": body,
                }}))
                .await =>
            {
                self.fake_id(previous)
            }
            None => {
                let Some(proxy) = self.bus().await else {
                    return;
                };
                let hints = HashMap::from([("urgency", Value::U8(level(urgency)))]);
                // A server that reads markup in the body must get the text escaped.
                let markup = self.markup.get();
                let text = |t: &str| if markup { escape(t) } else { t.to_string() };
                let (app, icon, title, body) = match origin {
                    None => (
                        APP.to_string(),
                        "utilities-terminal",
                        title.to_string(),
                        text(body),
                    ),
                    Some(o) => (
                        format!("{APP} · {}", o.host),
                        "network-server",
                        format!("シ SSH · {o}"),
                        if markup {
                            format!("<b>{}</b>\n{}", escape(title), escape(body))
                        } else {
                            format!("{title}\n{body}")
                        },
                    ),
                };
                let args = (
                    app.as_str(),
                    previous.unwrap_or(0),
                    icon,
                    title.as_str(),
                    body.as_str(),
                    vec!["default", "Open"],
                    hints,
                    -1i32,
                );
                match proxy.call::<_, _, u32>("Notify", &args).await {
                    Ok(id) => id,
                    Err(e) => {
                        self.failed_call("notify", e).await;
                        return;
                    }
                }
            }
        };
        self.ids.borrow_mut().insert(pane.to_string(), id);
        self.save.notify_one();
    }

    /// Closes `pane`'s notification, if it has one open.
    pub async fn close(&self, pane: &str) {
        let Some(id) = self.ids.borrow_mut().remove(pane) else {
            return;
        };
        self.save.notify_one();
        match &self.sink {
            Some(sink) => {
                let line =
                    serde_json::json!({"t": now_ms(), "effect": "notify-close", "pane": pane});
                append_line(sink, &line.to_string());
            }
            None if self.bridged(json!({"close": {"pane": pane}})).await => {}
            None => {
                if let Some(proxy) = self.bus().await
                    && let Err(e) = proxy.call::<_, _, ()>("CloseNotification", &(id,)).await
                {
                    self.failed_call("close notification", e).await;
                }
            }
        }
    }

    /// Sent over the desktop bridge, if there is one.
    async fn bridged(&self, message: serde_json::Value) -> bool {
        match &self.link {
            Some(link) => link.send(&message).await,
            None => false,
        }
    }

    /// Closes every open notification: the daemon goes for good (rollback),
    /// and nobody would close them when their panes are seen.
    pub async fn close_all(&self) {
        let panes: Vec<String> = self.ids.borrow().keys().cloned().collect();
        for pane in panes {
            self.close(&pane).await;
        }
    }

    /// The session bus, connected on first use, with its signals watched
    /// from before the first notification (a quick click is not missed).
    /// Without a bus, connecting is tried again on a later call, at most
    /// once every 30 s.
    async fn bus(&self) -> Option<Proxy<'static>> {
        let mut bus = self.bus.lock().await;
        if let Some(proxy) = bus.as_ref() {
            return Some(proxy.clone());
        }
        if self
            .failed
            .get()
            .is_some_and(|at| at.elapsed() < self.retry)
        {
            return None;
        }
        self.attempts.set(self.attempts.get() + 1);
        match self.connect().await {
            Ok(proxy) => {
                self.failed.set(None);
                *bus = Some(proxy.clone());
                Some(proxy)
            }
            Err(e) => {
                super::log(&format!("no notifications (D-Bus): {e}"));
                self.failed.set(Some(Instant::now()));
                None
            }
        }
    }

    /// A call failed; a connection that broke is dropped so the next call
    /// connects again.
    async fn failed_call(&self, what: &str, e: zbus::Error) {
        super::log(&format!("{what}: {e}"));
        if matches!(e, zbus::Error::InputOutput(_)) {
            *self.bus.lock().await = None;
        }
    }

    async fn connect(&self) -> zbus::Result<Proxy<'static>> {
        let conn = match &self.address {
            None => Connection::session().await?,
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await?
            }
        };
        let proxy: Proxy<'static> = Builder::new(&conn)
            .destination(SERVICE)?
            .path(PATH)?
            .interface(SERVICE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let caps: Vec<String> = proxy.call("GetCapabilities", &()).await.unwrap_or_default();
        self.markup.set(caps.iter().any(|c| c == "body-markup"));
        let mut actions = proxy.receive_signal("ActionInvoked").await?;
        let mut closed = proxy.receive_signal("NotificationClosed").await?;
        let (ids, jump, save) = (self.ids.clone(), self.jump.clone(), self.save.clone());
        let on_click = self.on_click.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    Some(msg) = next(&mut actions) => {
                        let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() else { continue };
                        let Some(pane) = forget(&ids, id) else { continue };
                        save.notify_one();
                        if action == "default" {
                            match &on_click {
                                Some(f) => f(&pane),
                                None => {
                                    let (bin, tmux) = jump.borrow().clone();
                                    clicked(&bin, &tmux, &pane);
                                }
                            }
                        }
                    }
                    Some(msg) = next(&mut closed) => {
                        let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() else { continue };
                        if forget(&ids, id).is_some() {
                            save.notify_one();
                        }
                    }
                    else => break,
                }
            }
        });
        Ok(proxy)
    }
}

/// Where a notification from another host comes from: `user@host`, or the
/// host alone.
#[derive(Debug, Clone, Copy)]
pub struct Origin<'a> {
    pub host: &'a str,
    pub user: Option<&'a str>,
}

impl std::fmt::Display for Origin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.user {
            Some(user) => write!(f, "{user}@{}", self.host),
            None => f.write_str(self.host),
        }
    }
}

/// Text for a body the server reads as markup (`cd x && make`, `a < b`).
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The pane whose open notification is `id`, forgotten.
fn forget(ids: &RefCell<BTreeMap<String, u32>>, id: u32) -> Option<String> {
    let mut ids = ids.borrow_mut();
    let pane = ids
        .iter()
        .find(|(_, v)| **v == id)
        .map(|(p, _)| p.clone())?;
    ids.remove(&pane);
    Some(pane)
}

/// N3: clicking a notification brings its pane in front.
fn clicked(bin: &str, tmux: &str, pane: &str) {
    if bin.is_empty() {
        return;
    }
    let child = Command::new(format!("{bin}/agent-jump"))
        .arg(pane)
        .env("TMUX", tmux)
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Ok(mut child) = child {
        tokio::task::spawn_local(async move {
            let _ = child.wait().await;
        });
    }
}

fn level(urgency: Urgency) -> u8 {
    match urgency {
        Urgency::Low => 0,
        Urgency::Normal => 1,
        Urgency::Critical => 2,
    }
}

async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test(flavor = "current_thread")]
    async fn n3_sink_keeps_one_per_pane() {
        let dir = std::env::temp_dir().join(format!("agentd-notify-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let sink = dir.join("sink.jsonl");
        let n = Notifier::new(
            Some(sink.clone()),
            BTreeMap::new(),
            Rc::new(Notify::new()),
            None,
        );
        n.close("%1").await; // nothing open: no line
        n.show(
            "%1",
            Urgency::Critical,
            "api · Fix",
            "Needs permission: Bash: ls",
            "",
            "",
        )
        .await;
        let first = n.ids()["%1"];
        n.show("%1", Urgency::Normal, "api", "Done", "", "").await;
        assert_eq!(n.ids()["%1"], first, "a replacement keeps the id");
        n.show("%2", Urgency::Normal, "web", "Done", "", "").await;
        assert_ne!(n.ids()["%2"], first);
        n.close("%1").await;
        assert!(!n.ids().contains_key("%1"));
        let lines: Vec<serde_json::Value> = fs::read_to_string(&sink)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let effects: Vec<(&str, &str)> = lines
            .iter()
            .map(|l| (l["effect"].as_str().unwrap(), l["pane"].as_str().unwrap()))
            .collect();
        assert_eq!(
            effects,
            [
                ("notify", "%1"),
                ("notify", "%1"),
                ("notify", "%2"),
                ("notify-close", "%1")
            ]
        );
        assert_eq!(lines[0]["urgency"], "critical");
        assert_eq!(lines[0]["title"], "api · Fix");
        let raw = fs::read_to_string(&sink).unwrap();
        assert!(raw.starts_with("{\"t\":"), "{raw}");
        assert!(raw.contains("api · Fix"), "not ASCII-escaped: {raw}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_bus_is_tried_again_later() {
        let mut n = Notifier::new(None, BTreeMap::new(), Rc::new(Notify::new()), None);
        n.address = Some("unix:path=/nonexistent/agentd-test/bus".into());
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                n.show("%1", Urgency::Normal, "t", "b", "", "").await;
                assert_eq!(n.attempts.get(), 1);
                assert!(n.ids().is_empty(), "nothing shown");
                // Within the retry interval: not again.
                n.close("%1").await;
                n.show("%1", Urgency::Normal, "t", "b", "", "").await;
                assert_eq!(n.attempts.get(), 1);
                // Once it has passed: again.
                n.retry = Duration::ZERO;
                n.show("%1", Urgency::Normal, "t", "b", "", "").await;
                assert_eq!(n.attempts.get(), 2);
            })
            .await;
    }

    #[test]
    fn origin_and_escape() {
        let o = Origin {
            host: "box",
            user: Some("ana"),
        };
        assert_eq!(o.to_string(), "ana@box");
        assert_eq!(Origin { user: None, ..o }.to_string(), "box");
        assert_eq!(escape("cd x && a <b>"), "cd x &amp;&amp; a &lt;b&gt;");
    }

    #[test]
    fn n3_forget_by_id() {
        let ids = RefCell::new(BTreeMap::from([
            ("%1".to_string(), 7),
            ("%2".to_string(), 9),
        ]));
        assert_eq!(forget(&ids, 9).as_deref(), Some("%2"));
        assert_eq!(forget(&ids, 9), None);
        assert_eq!(ids.borrow().len(), 1);
    }
}
