//! Text the contract spells out: labels, titles, subagent types.

use std::collections::BTreeMap;

/// I3: every field is cut to this many characters.
pub const MAX_CHARS: usize = 300;

/// I3's `gsub("\\s+"; " ") | .[0:300]`: whitespace runs (Unicode, as jq's
/// `\s`) collapsed to one space, then cut to 300 characters.
pub fn line(text: &str) -> String {
    let mut out = String::new();
    let mut chars = 0;
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if in_space {
                continue;
            }
            in_space = true;
            out.push(' ');
        } else {
            in_space = false;
            out.push(c);
        }
        chars += 1;
        if chars == MAX_CHARS {
            break;
        }
    }
    out
}

/// tmux expands `#` in the formats that show these options: store `##`.
pub fn escape_hashes(s: &str) -> String {
    s.replace('#', "##")
}

pub fn unescape_hashes(s: &str) -> String {
    s.replace("##", "#")
}

/// Tool label (I3): `tool` alone when there is no detail, else `tool: detail`.
pub fn tool_label(tool: &str, detail: &str) -> String {
    if detail.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}: {detail}")
    }
}

/// Task name from a pane title (N2): Claude sets "✳ topic", Codex
/// "topic | project"; the last " | ..." part goes.
fn task(title: &str) -> &str {
    let t = title.strip_prefix("✳ ").unwrap_or(title);
    match t.rfind(" | ") {
        Some(i) => &t[..i],
        None => t,
    }
}

/// Notification title (N2): the session, plus " · <task>" unless the task
/// is empty or the host name.
pub fn notify_title(session: &str, pane_title: &str, host: &str) -> String {
    let task = task(pane_title);
    if task.is_empty() || task == host {
        session.to_string()
    } else {
        format!("{session} · {task}")
    }
}

/// `@agent_subtypes` (A2): "N type" per type, sorted by name ignoring case,
/// ties in byte order. Empty or `default` types count as `agent`.
pub fn subtypes<'a>(types: impl IntoIterator<Item = &'a String>) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for t in types {
        let t = if t.is_empty() || t == "default" {
            "agent"
        } else {
            t.as_str()
        };
        *counts.entry(t).or_default() += 1;
    }
    let mut entries: Vec<(&str, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then(a.0.cmp(b.0))
    });
    entries
        .iter()
        .map(|(t, n)| format!("{n} {t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i3_label() {
        assert_eq!(tool_label("Bash", "ls"), "Bash: ls");
        assert_eq!(tool_label("Bash", ""), "Bash");
        assert_eq!(tool_label("", "ls"), ": ls");
    }

    #[test]
    fn n2_task_from_title() {
        assert_eq!(task("✳ Fix CSV"), "Fix CSV");
        assert_eq!(task("Fix CSV | api"), "Fix CSV");
        assert_eq!(task("a | b | c"), "a | b");
        assert_eq!(task("✳ ✳ x"), "✳ x");
        assert_eq!(task("plain"), "plain");
    }

    #[test]
    fn n2_title() {
        assert_eq!(notify_title("api", "✳ Fix CSV", "box"), "api · Fix CSV");
        assert_eq!(notify_title("api", "box", "box"), "api");
        assert_eq!(notify_title("api", "✳ box", "box"), "api");
        assert_eq!(notify_title("api", "", "box"), "api");
    }

    #[test]
    fn a2_subtypes_order_and_names() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            subtypes(&v(&["Plan", "Explore", "", "Explore"])),
            "1 agent, 2 Explore, 1 Plan"
        );
        assert_eq!(
            subtypes(&v(&["default", "zeta", "Alpha"])),
            "1 agent, 1 Alpha, 1 zeta"
        );
        // Same name up to case: byte order breaks the tie, upper case first.
        assert_eq!(subtypes(&v(&["plan", "Plan"])), "1 Plan, 1 plan");
        assert_eq!(subtypes(&v(&[])), "");
    }

    #[test]
    fn hashes() {
        assert_eq!(escape_hashes("a#b##"), "a##b####");
        assert_eq!(unescape_hashes("a##b####"), "a#b##");
    }
}
