//! Hook events (contract §3, §5, §6, §8, §11), in the order agent-hook
//! applies them at 885bb9b. Codex goes through `codex::prepare` first, which
//! may turn the event into another (an observation that ends the turn) and
//! decides the state; the rest is shared.

use std::cell::RefCell;

use regex::Regex;

use super::codex::{self, Update};
use super::text::{escape_hashes, notify_title, subtypes, tool_label, unescape_hashes};
use super::{ALL_OPTIONS, Effect, Facts, Input, Kind, Op, State, Urgency, Visibility};

/// S8: the default test regex of agent-hook.
const TEST_REGEX: &str = r"(^|[[:space:];&|(])((npm|pnpm|yarn|bun)( run)? test|npx (vitest|jest)|vitest|jest|pytest|go test|cargo (test|nextest)|make (test|check)|mix test|dotnet test|rspec|phpunit|ctest|deno test|lake test|python3? -m (pytest|unittest))([[:space:];&|):]|$)";

/// S6: at most one test-run sound per pane every 10 minutes.
const TESTS_SOUND_EVERY: i64 = 600;

/// N4: default `@agent_remind_after`.
const REMIND_AFTER: f64 = 900.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Needs {
    Permission,
    Question,
    Plan,
}

impl Needs {
    fn as_str(self) -> &'static str {
        match self {
            Needs::Permission => "permission",
            Needs::Question => "question",
            Needs::Plan => "plan",
        }
    }
}

struct Writes(Vec<Op>);

impl Writes {
    fn set(&mut self, name: &'static str, value: impl Into<String>) {
        self.0.push(Op::Set(name, value.into()));
    }
    fn unset(&mut self, name: &'static str) {
        self.0.push(Op::Unset(name));
    }
    /// A2: `@agent_subs` and `@agent_subtypes` from the session's subagents.
    fn subs(&mut self, state: &State, sid: &str) {
        let running = state.subagents.get(sid);
        let n = running.map_or(0, |m| m.len());
        self.set("@agent_subs", n.to_string());
        match running {
            Some(m) if n > 0 => self.set("@agent_subtypes", subtypes(m.values())),
            _ => self.unset("@agent_subtypes"),
        }
    }
}

pub fn handle(state: &mut State, input: &Input, facts: &Facts) -> Vec<Effect> {
    let codex: Option<Update> = match input.kind {
        Kind::Claude => None,
        Kind::Codex => {
            let Some(cf) = &facts.codex else {
                return Vec::new();
            };
            match codex::prepare(
                state,
                &input.pane,
                &input.event,
                &facts.pane,
                cf,
                input.agent_pid,
                input.observing,
            ) {
                Some(update) => Some(update),
                None => return Vec::new(), // X1: another session or a stale turn
            }
        }
    };
    let e = codex.as_ref().map_or(&input.event, |u| &u.event);
    let pane = &facts.pane;
    let cur = pane.state.as_str();
    let mut w = Writes(Vec::new());

    // A1: a subagent's own events only touch the subagent count.
    if !e.agent_id.is_empty() {
        match e.ev.as_str() {
            "SubagentStart" => {
                state
                    .subagents
                    .entry(e.sid.clone())
                    .or_default()
                    .insert(e.agent_id.clone(), e.agent_type.clone());
            }
            "SubagentStop" => {
                if let Some(m) = state.subagents.get_mut(&e.sid) {
                    m.remove(&e.agent_id);
                    if m.is_empty() {
                        state.subagents.remove(&e.sid);
                    }
                }
            }
            _ => return Vec::new(),
        }
        w.subs(state, &e.sid);
        return vec![Effect::Options(w.0)];
    }

    let label = tool_label(&e.tool, &e.detail);
    let mut new_state: Option<String> = None;
    let mut needs: Option<Needs> = None;
    let mut bg_shells = 0;
    let mut effects = Vec::new();

    match e.ev.as_str() {
        "SessionStart" => {
            if !e.model.is_empty() {
                w.set("@agent_model", e.model.clone()); // H1c
            }
            if e.source != "compact" {
                // H1, A3: a fresh session forgets the subagents of both.
                new_state = Some("ready".into());
                if !pane.sid.is_empty() {
                    state.subagents.remove(&pane.sid);
                }
                state.subagents.remove(&e.sid);
                w.subs(state, &e.sid);
                w.unset("@agent_tool");
                w.unset("@agent_msg");
            }
        }
        "UserPromptSubmit" => {
            new_state = Some("working".into()); // H2
            w.unset("@agent_tool");
            w.unset("@agent_msg");
        }
        "PreToolUse" => {
            if cur != "needs" {
                new_state = Some("working".into()); // H3
            }
            w.set("@agent_tool", escape_hashes(&label));
        }
        "PermissionRequest" => {
            let kind = match e.tool.as_str() {
                "AskUserQuestion" => Needs::Question,
                "ExitPlanMode" => Needs::Plan,
                _ => Needs::Permission,
            };
            if kind == Needs::Permission && e.mode == "bypassPermissions" {
                // H4b: asked, but the mode answers it; a dialog shown anyway
                // comes as Notification permission_prompt (H6).
                if cur != "needs" {
                    new_state = Some("working".into());
                }
            } else {
                new_state = Some("needs".into()); // H4
                needs = Some(kind);
                w.set("@agent_needs_id", e.tool_id.clone());
            }
            w.set("@agent_tool", escape_hashes(&label));
        }
        "PostToolUse" | "PostToolUseFailure" => {
            // H5: with parallel calls, only the one that asked ends the wait.
            if cur != "needs" || pane.needs_id.is_empty() || e.tool_id == pane.needs_id {
                new_state = Some("working".into());
            }
        }
        "Notification" => match e.ntype.as_str() {
            "permission_prompt" => {
                if cur != "needs" {
                    new_state = Some("needs".into()); // H6
                    needs = Some(Needs::Permission);
                }
            }
            "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input" => {
                new_state = Some("needs".into()); // H6b
                needs = Some(Needs::Question);
            }
            _ => return Vec::new(), // H6c
        },
        "Elicitation" => {
            new_state = Some("needs".into()); // H7
            needs = Some(Needs::Question);
        }
        "ElicitationResult" => {
            if cur == "needs" {
                new_state = Some("working".into()); // H7b
            }
        }
        "PreCompact" => {
            if cur != "compacting" {
                w.set("@agent_prev", cur); // H8
            }
            new_state = Some("compacting".into());
        }
        "PostCompact" => {
            // H8b
            new_state = Some(match pane.prev.as_str() {
                "" | "compacting" => "idle".into(),
                prev => prev.into(),
            });
        }
        "Stop" => {
            new_state = Some("done".into()); // H9
            w.set("@agent_msg", escape_hashes(&e.last));
            w.unset("@agent_tool");
            match &codex {
                // B1: shells left running in the background.
                None => {
                    bg_shells = facts.bg_shells;
                    if bg_shells > 0 {
                        // Counted now; the daemon keeps it up to date (B1).
                        w.set("@agent_bg", bg_shells.to_string());
                        effects.push(Effect::Bgwatch {
                            agent_pid: input.agent_pid,
                        });
                    }
                }
                // X7: the command trees just counted.
                Some(u) => {
                    bg_shells = u
                        .options
                        .iter()
                        .find(|(n, _)| *n == "@agent_bg")
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                }
            }
        }
        "Interrupt" if codex.is_some() => {
            new_state = Some("idle".into()); // X3
            w.unset("@agent_tool");
            w.unset("@agent_prev");
        }
        "CodexReconcile" if codex.is_some() => {}
        "StopFailure" => {
            new_state = Some("error".into()); // H10
            let text = [&e.error, &e.last]
                .into_iter()
                .find(|s| !s.is_empty())
                .map_or("unknown error", |s| s.as_str());
            w.set("@agent_msg", escape_hashes(text));
        }
        "SessionEnd" => {
            // H11: clear the pane, forget its subagents, close its notification.
            state.subagents.remove(&e.sid);
            for name in ALL_OPTIONS {
                w.unset(name);
            }
            return vec![
                Effect::Options(w.0),
                Effect::NotifyClose,
                Effect::RemindCancel,
            ];
        }
        _ => return Vec::new(), // H12, and Codex-only events
    }

    // Codex decides the state (X3) and has its own options (X6).
    if let Some(u) = &codex {
        new_state = (!u.state.is_empty()).then(|| u.state.clone());
        needs = match u.needs.as_str() {
            "permission" => Some(Needs::Permission),
            "question" => Some(Needs::Question),
            _ => None,
        };
        for (name, value) in &u.options {
            if value.is_empty() {
                w.unset(name);
            } else {
                w.set(name, value.clone());
            }
        }
    }

    // H14: identity.
    w.set("@agent", input.kind.as_str());
    if input.kind == Kind::Claude {
        w.set("@agent_profile", profile(input.config_dir.as_deref()));
    }
    if !e.sid.is_empty() {
        w.set("@agent_session", e.sid.clone());
    }
    if !e.transcript.is_empty() {
        w.set("@agent_transcript", e.transcript.clone());
    }
    if !e.mode.is_empty() {
        w.set("@agent_mode", e.mode.clone());
    }

    let muted = pane.muted;
    let sound = |effects: &mut Vec<Effect>, name: &'static str| {
        if !muted {
            effects.push(Effect::Sound(name));
        }
    };

    match e.ev.as_str() {
        // R: the pane joins its space's round.
        "UserPromptSubmit" => {
            state
                .rounds
                .entry(pane.space.clone())
                .or_default()
                .insert(input.pane.clone());
        }
        // S5: you just accepted the plan.
        "PostToolUse" if e.tool == "ExitPlanMode" => sound(&mut effects, "lets-do-this"),
        // S6: at most every 10 minutes per pane; agents run tests a lot.
        "PreToolUse" if e.tool == "Bash" && is_test_run(&e.detail, &facts.test_regex) => {
            let at = pane.tests_sound_at.parse::<i64>().unwrap_or(0);
            if facts.now - at >= TESTS_SOUND_EVERY {
                sound(&mut effects, "fight-like-a-man");
                w.set("@agent_tests_sound_at", facts.now.to_string());
            }
        }
        _ => {}
    }

    // N3: leaving needs closes its notification; C1: and cancels its reminder.
    if cur == "needs" && new_state.as_deref().is_some_and(|s| s != "needs") {
        effects.insert(0, Effect::NotifyClose);
        effects.insert(1, Effect::RemindCancel);
    }

    let entering_needs = new_state.as_deref() == Some("needs") && cur != "needs";
    if matches!(new_state.as_deref(), Some("done" | "error")) || entering_needs {
        // V1, V2. Not computed means the daemon judged it unneeded: away.
        let vis = facts.vis.unwrap_or(Visibility::Away);
        if new_state.as_deref() == Some("done") && vis == Visibility::Visible {
            new_state = Some("idle".into()); // H9: seen already
        }
        let subs = state.subagents.get(&e.sid).map_or(0, |m| m.len());
        match e.ev.as_str() {
            "Stop" => {
                if subs == 0 {
                    if round_won(state, &input.pane, &pane.space, facts) {
                        sound(&mut effects, "ct-win"); // S2
                    } else if vis != Visibility::Visible {
                        sound(&mut effects, "enemy-down"); // S3
                    }
                }
            }
            "StopFailure" => sound(&mut effects, "oh-man"), // S4
            _ => {
                if new_state.as_deref() == Some("needs") {
                    if vis != Visibility::Visible {
                        // S1
                        sound(
                            &mut effects,
                            match needs {
                                Some(Needs::Question) => "report-in",
                                Some(Needs::Plan) => "wait-for-my-go",
                                _ => "need-backup",
                            },
                        );
                    }
                    effects.push(Effect::RemindArm {
                        after: remind_after(&facts.remind_after),
                        since: facts.now, // C2: the @agent_since written below
                    });
                }
            }
        }
        // N1, N2
        let done_with_subs = new_state.as_deref() == Some("done") && subs > 0;
        if vis == Visibility::Away && !muted && !done_with_subs {
            let (urgency, body) = match new_state.as_deref() {
                Some("needs") => (
                    Urgency::Critical,
                    match needs {
                        Some(Needs::Question) => "Has a question for you".to_string(),
                        Some(Needs::Plan) => "Plan ready for your review".to_string(),
                        _ => {
                            let label = if e.ev == "PermissionRequest" {
                                label.clone()
                            } else {
                                unescape_hashes(&pane.tool)
                            };
                            if label.is_empty() {
                                "Needs permission".to_string()
                            } else {
                                format!("Needs permission: {label}")
                            }
                        }
                    },
                ),
                Some("done") => {
                    let mut body = if e.last.is_empty() {
                        "Finished".to_string()
                    } else {
                        e.last.clone()
                    };
                    if bg_shells > 0 {
                        let s = if bg_shells > 1 { "s" } else { "" };
                        body.push_str(&format!(" · {bg_shells} shell{s} still running"));
                    }
                    (Urgency::Normal, body)
                }
                _ => (
                    Urgency::Critical,
                    format!(
                        "Stopped: {}",
                        if e.error.is_empty() {
                            "error"
                        } else {
                            &e.error
                        }
                    ),
                ),
            };
            effects.push(Effect::Notify {
                urgency,
                title: notify_title(&pane.session, &pane.title, &facts.host),
                body,
            });
        }
    }

    // H13: writing the state.
    if let Some(s) = new_state.as_deref() {
        if s != cur {
            w.set("@agent_since", facts.now.to_string());
        }
        w.set("@agent_state", s);
        if s == "needs" {
            if let Some(n) = needs {
                w.set("@agent_needs", n.as_str());
            }
        } else {
            w.unset("@agent_needs");
            w.unset("@agent_needs_id");
        }
    }

    // H5b: a Claude permission dialog (H4, H6) is answered without a hook;
    // the daemon watches for the command it lets run.
    if input.kind == Kind::Claude
        && new_state.as_deref() == Some("needs")
        && needs == Some(Needs::Permission)
    {
        let since = if cur == "needs" {
            pane.since.parse().unwrap_or(facts.now)
        } else {
            facts.now
        };
        effects.push(Effect::AwaitAnswer { since });
    }

    // K5: the turn signal, once the state is written.
    if matches!(new_state.as_deref(), Some("needs" | "done")) {
        effects.push(Effect::Blink);
    }
    // X5: a Codex hook (not an observation) makes sure someone observes.
    if input.kind == Kind::Codex && !input.observing && input.event.agent_id.is_empty() {
        effects.push(Effect::Watch);
    }

    let mut out = vec![Effect::Options(w.0)];
    out.extend(effects);
    out
}

/// H14: basename of `CLAUDE_CONFIG_DIR` (default `~/.claude`) without a
/// trailing slash; `.claude` is shown as `default`.
fn profile(config_dir: Option<&str>) -> String {
    let dir = match config_dir {
        Some(d) if !d.is_empty() => d,
        _ => return "default".into(),
    };
    let dir = dir.strip_suffix('/').unwrap_or(dir);
    let base = dir.rsplit('/').next().unwrap_or(dir);
    if base == ".claude" {
        "default".into()
    } else {
        base.into()
    }
}

/// R: at a Stop with no subagents, the round goes on while another pane of
/// the space is busy; otherwise it ends, won with at least two panes.
fn round_won(state: &mut State, me: &str, space: &str, facts: &Facts) -> bool {
    let busy = facts.others.iter().any(|p| {
        p.pane != me
            && p.space == space
            && (matches!(p.state.as_str(), "working" | "needs" | "compacting")
                || p.subs.parse::<i64>().is_ok_and(|n| n > 0))
    });
    if busy {
        return false;
    }
    state.rounds.remove(space).map_or(0, |r| r.len()) >= 2
}

/// S8: does a Bash call run a test suite? `@agent_test_regex` overrides; an
/// invalid one matches nothing.
fn is_test_run(detail: &str, custom: &str) -> bool {
    thread_local! {
        // The last pattern compiled: it changes only when the user sets it.
        static COMPILED: RefCell<Option<(String, Option<Regex>)>> = const { RefCell::new(None) };
    }
    let pattern = if custom.is_empty() {
        TEST_REGEX
    } else {
        custom
    };
    COMPILED.with_borrow_mut(|cached| {
        if cached.as_ref().is_none_or(|(p, _)| p != pattern) {
            *cached = Some((pattern.to_string(), Regex::new(pattern).ok()));
        }
        cached
            .as_ref()
            .and_then(|(_, re)| re.as_ref())
            .is_some_and(|re| re.is_match(detail))
    })
}

/// N4: `@agent_remind_after`, default 900. Bash hands it to GNU `sleep`, so
/// a suffix `s`, `m`, `h` or `d` works too. Empty or unreadable: 900 (C7).
fn remind_after(raw: &str) -> f64 {
    let (number, unit) = match raw.char_indices().last() {
        Some((i, 's')) => (&raw[..i], 1.0),
        Some((i, 'm')) => (&raw[..i], 60.0),
        Some((i, 'h')) => (&raw[..i], 3600.0),
        Some((i, 'd')) => (&raw[..i], 86400.0),
        _ => (raw, 1.0),
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map_or(REMIND_AFTER, |n| n * unit)
}
