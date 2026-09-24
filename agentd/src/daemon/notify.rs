//! N1-N3 over D-Bus, as agents/bin/agent-notify does, in-process: one
//! session connection, one match for the notification signals, one open
//! notification per pane (its id in memory and in the state file, never in
//! pane options: C3). A click runs `agent-jump <pane>`.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::future::poll_fn;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::sync::{Notify, OnceCell};
use zbus::export::futures_core::Stream;
use zbus::proxy::{Builder, CacheProperties};
use zbus::zvariant::Value;
use zbus::{Connection, Proxy};

use super::sound::append_line;
use crate::core::Urgency;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

pub struct Notifier {
    /// `AG_SINK` (tests): lines instead of D-Bus, which is never touched.
    sink: Option<PathBuf>,
    bus: OnceCell<Option<Proxy<'static>>>,
    /// The open notification of each pane.
    ids: Rc<RefCell<BTreeMap<String, u32>>>,
    /// Sink mode's ids (a notification server's own counter).
    next_fake: RefCell<u32>,
    /// `@agents_bin` and `$TMUX`, for agent-jump on a click.
    jump: Rc<RefCell<(String, String)>>,
    save: Rc<Notify>,
}

impl Notifier {
    pub fn new(sink: Option<PathBuf>, ids: BTreeMap<String, u32>, save: Rc<Notify>) -> Notifier {
        Notifier {
            sink,
            bus: OnceCell::new(),
            ids: Rc::new(RefCell::new(ids)),
            next_fake: RefCell::new(1),
            jump: Rc::new(RefCell::new((String::new(), String::new()))),
            save,
        }
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
        let previous = self.ids.borrow().get(pane).copied();
        let id = match &self.sink {
            Some(sink) => {
                let line = serde_json::json!({
                    "t": now_ms(), "effect": "notify", "pane": pane,
                    "urgency": urgency.as_str(), "title": title, "body": body,
                });
                append_line(sink, &line.to_string());
                // A notification server keeps the id of one it replaces.
                previous.unwrap_or_else(|| {
                    let mut next = self.next_fake.borrow_mut();
                    *next += 1;
                    *next
                })
            }
            None => {
                let Some(proxy) = self.bus().await else {
                    return;
                };
                let hints = HashMap::from([("urgency", Value::U8(level(urgency)))]);
                let args = (
                    "tmux agents",
                    previous.unwrap_or(0),
                    "utilities-terminal",
                    title,
                    body,
                    vec!["default", "Open"],
                    hints,
                    -1i32,
                );
                match proxy.call::<_, _, u32>("Notify", &args).await {
                    Ok(id) => id,
                    Err(e) => {
                        super::log(&format!("notify: {e}"));
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
            None => {
                if let Some(proxy) = self.bus().await
                    && let Err(e) = proxy.call::<_, _, ()>("CloseNotification", &(id,)).await
                {
                    super::log(&format!("close notification: {e}"));
                }
            }
        }
    }

    /// The session bus, connected on first use, with its signals watched
    /// from before the first notification (a quick click is not missed).
    async fn bus(&self) -> Option<&Proxy<'static>> {
        self.bus
            .get_or_init(|| async {
                match self.connect().await {
                    Ok(proxy) => Some(proxy),
                    Err(e) => {
                        super::log(&format!("no notifications (D-Bus): {e}"));
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    async fn connect(&self) -> zbus::Result<Proxy<'static>> {
        let conn = Connection::session().await?;
        let proxy: Proxy<'static> = Builder::new(&conn)
            .destination(SERVICE)?
            .path(PATH)?
            .interface(SERVICE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let mut actions = proxy.receive_signal("ActionInvoked").await?;
        let mut closed = proxy.receive_signal("NotificationClosed").await?;
        let (ids, jump, save) = (self.ids.clone(), self.jump.clone(), self.save.clone());
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    Some(msg) = next(&mut actions) => {
                        let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() else { continue };
                        let Some(pane) = forget(&ids, id) else { continue };
                        save.notify_one();
                        if action == "default" {
                            let (bin, tmux) = jump.borrow().clone();
                            clicked(&bin, &tmux, &pane);
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
        let n = Notifier::new(Some(sink.clone()), BTreeMap::new(), Rc::new(Notify::new()));
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
