//! The event log (internals.md, "Debugging"): one line per input the daemon
//! handles, always on, so a glitch ("it showed ▲ while nothing waited") can
//! be looked into afterwards. Only structural fields go in: event names,
//! modes, types, tool names, whether an id came, and the state before and
//! after. Never prompts, commands, paths, messages or anything else an agent
//! or the user wrote. Bounded: at 1 MiB the file becomes `events.log.1`.
//! A failed write changes nothing else.

use std::cell::{Cell, OnceCell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::zone::{Zone, civil_from_days};
use crate::core::{Event, Op, Pane};

/// Rotated past this size, keeping one old file.
const MAX_SIZE: u64 = 1 << 20;
/// A structural value longer than this is not one.
const MAX_TOKEN: usize = 64;

pub struct EventLog {
    /// `None`: no state folder, nothing is logged.
    path: Option<PathBuf>,
    /// The tmux socket's name: several servers share the file.
    server: String,
    file: RefCell<Option<File>>,
    /// What we know the file holds (it is ours to rotate at `MAX_SIZE`).
    size: Cell<u64>,
    /// The local zone, read at the first line (`None`: UTC).
    zone: OnceCell<Option<Zone>>,
}

impl EventLog {
    pub fn new(dir: Option<PathBuf>, server: &str) -> Self {
        EventLog {
            path: dir.map(|d| d.join("events.log")),
            server: token(server),
            file: RefCell::new(None),
            size: Cell::new(0),
            zone: OnceCell::new(),
        }
    }

    /// One line: time, server, pane, kind, then `what` (built here from
    /// structural fields only).
    pub fn line(&self, pane: &str, kind: &str, what: &str) {
        let Some(path) = &self.path else { return };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let text = format!(
            "{} {} {} {} {what}\n",
            local_time(now, self.offset(now.div_euclid(1000))),
            self.server,
            token(pane),
            token(kind),
        );
        let mut file = self.file.borrow_mut();
        if file.is_some() && self.size.get() + text.len() as u64 > MAX_SIZE {
            let ours = file.as_ref().and_then(|f| f.metadata().ok());
            *file = None;
            // Another daemon (another tmux server) may have rotated it
            // already: then only reopen.
            let same = fs::metadata(path)
                .ok()
                .zip(ours)
                .is_some_and(|(p, o)| (p.dev(), p.ino()) == (o.dev(), o.ino()));
            if same {
                let _ = fs::rename(path, path.with_extension("log.1"));
            }
        }
        if file.is_none() {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            *file = OpenOptions::new().create(true).append(true).open(path).ok();
            self.size.set(
                file.as_ref()
                    .and_then(|f| f.metadata().ok())
                    .map_or(0, |m| m.len()),
            );
        }
        let Some(f) = file.as_mut() else { return };
        if f.write_all(text.as_bytes()).is_ok() {
            self.size.set(self.size.get() + text.len() as u64);
        } else {
            *file = None; // opened again next time
        }
    }

    /// Seconds east of UTC at `now` (epoch seconds).
    fn offset(&self, now: i64) -> i64 {
        self.zone
            .get_or_init(Zone::local)
            .as_ref()
            .map_or(0, |z| z.offset(now))
    }
}

/// `2026-09-25 11:00:47.123` for epoch milliseconds at a UTC offset.
fn local_time(ms: i64, offset: i64) -> String {
    let local = ms.div_euclid(1000) + offset;
    let (days, secs) = (local.div_euclid(86400), local.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ms.rem_euclid(1000)
    )
}

/// A structural value as one word: whatever is not a plain identifier
/// character becomes `_`, cut to `MAX_TOKEN` characters; empty is `-`.
fn token(value: &str) -> String {
    if value.is_empty() {
        return "-".into();
    }
    value
        .chars()
        .take(MAX_TOKEN)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:%@/+-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A hook event: its name and the structural fields it carried. The event's
/// texts (detail, last, error, transcript, session) never appear.
pub fn hook(e: &Event) -> String {
    let mut out = token(&e.ev);
    let mut field = |name: &str, value: &str| {
        if !value.is_empty() {
            out.push_str(&format!(" {name}={}", token(value)));
        }
    };
    field("mode", &e.mode);
    field("type", &e.ntype);
    field("source", &e.source);
    field("tool", &e.tool);
    if !e.tool.is_empty() || e.ev.contains("Tool") || e.ev == "PermissionRequest" {
        field(
            "tool_use_id",
            if e.tool_id.is_empty() { "no" } else { "yes" },
        );
    }
    if !e.agent_id.is_empty() {
        field("subagent", "yes");
    }
    out
}

/// `before->after`: the pane's state (with its needs kind) before the
/// input, and after the writes it made.
pub fn transition(before: &Pane, ops: &[Op]) -> String {
    let (mut state, mut needs) = (before.state.as_str(), before.needs.as_str());
    for op in ops {
        match op {
            Op::Set("@agent_state", v) => state = v,
            Op::Unset("@agent_state") => state = "",
            Op::Set("@agent_needs", v) => needs = v,
            Op::Unset("@agent_needs") => needs = "",
            _ => {}
        }
    }
    format!(
        "{}->{}",
        shown(&before.state, &before.needs),
        shown(state, needs)
    )
}

/// Whether the writes change the state or its needs kind.
pub fn changes(before: &Pane, ops: &[Op]) -> bool {
    let t = transition(before, ops);
    t.split_once("->").is_some_and(|(a, b)| a != b)
}

fn shown(state: &str, needs: &str) -> String {
    match (state, needs) {
        ("needs", n) if !n.is_empty() => format!("needs:{}", token(n)),
        ("", _) => "none".into(),
        (s, _) => token(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_format() {
        // 2026-09-25 09:00:47.123 UTC, at +02:00.
        assert_eq!(
            local_time(1_790_326_847_123, 7200),
            "2026-09-25 11:00:47.123"
        );
        assert_eq!(local_time(0, 0), "1970-01-01 00:00:00.000");
        assert_eq!(local_time(-1, 0), "1969-12-31 23:59:59.999");
        // 2024-02-29 23:30 UTC at -05:00.
        assert_eq!(
            local_time(1_709_249_400_000, -18000),
            "2024-02-29 18:30:00.000"
        );
    }

    #[test]
    fn tokens_are_one_word() {
        assert_eq!(token("bypassPermissions"), "bypassPermissions");
        assert_eq!(token("mcp__srv__tool"), "mcp__srv__tool");
        assert_eq!(token("a b\nc"), "a_b_c");
        assert_eq!(token(""), "-");
        assert_eq!(token(&"x".repeat(100)).len(), MAX_TOKEN);
    }

    #[test]
    fn hook_fields_are_structural_only() {
        let canary = "canary words";
        let e = Event {
            ev: "PermissionRequest".into(),
            sid: canary.into(),
            tool: "Bash".into(),
            detail: canary.into(),
            last: canary.into(),
            error: canary.into(),
            transcript: canary.into(),
            mode: "bypassPermissions".into(),
            ..Event::default()
        };
        assert_eq!(
            hook(&e),
            "PermissionRequest mode=bypassPermissions tool=Bash tool_use_id=no"
        );
        let e = Event {
            ev: "Notification".into(),
            ntype: "permission_prompt".into(),
            agent_id: "a1".into(),
            ..Event::default()
        };
        assert_eq!(hook(&e), "Notification type=permission_prompt subagent=yes");
    }

    #[test]
    fn transitions() {
        let before = Pane {
            state: "needs".into(),
            needs: "permission".into(),
            ..Pane::default()
        };
        let ops = [
            Op::Set("@agent_state", "working".into()),
            Op::Unset("@agent_needs"),
        ];
        assert_eq!(transition(&before, &ops), "needs:permission->working");
        assert!(changes(&before, &ops));
        assert_eq!(
            transition(&before, &[]),
            "needs:permission->needs:permission"
        );
        assert!(!changes(&before, &[]));
        let fresh = Pane::default();
        let ops = [Op::Set("@agent_state", "ready".into())];
        assert_eq!(transition(&fresh, &ops), "none->ready");
    }

    #[test]
    fn rotates_at_the_limit() {
        let dir = std::env::temp_dir().join(format!("agentd-evlog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = EventLog::new(Some(dir.clone()), "default");
        let what = "x".repeat(1000);
        for _ in 0..1100 {
            log.line("%1", "claude", &what);
        }
        let now = fs::metadata(dir.join("events.log")).unwrap().len();
        let old = fs::metadata(dir.join("events.log.1")).unwrap().len();
        assert!(old <= MAX_SIZE && old > MAX_SIZE - 2000, "{old}");
        assert!(now < MAX_SIZE, "{now}");
        let text = fs::read_to_string(dir.join("events.log")).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.contains(" default %1 claude xxx"), "{first}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
