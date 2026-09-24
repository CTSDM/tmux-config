//! Unit tests of the core, named after the contract's rule ids.

use super::*;

const NOW: i64 = 1_790_000_000;

fn pane() -> Pane {
    Pane {
        session: "api".into(),
        title: "✳ Fix CSV".into(),
        space: "work".into(),
        ..Pane::default()
    }
}

fn in_state(state: &str) -> Pane {
    Pane {
        state: state.into(),
        since: (NOW - 100).to_string(),
        ..pane()
    }
}

fn ev(name: &str) -> Event {
    Event {
        ev: name.into(),
        sid: "s1".into(),
        ..Event::default()
    }
}

fn tool_ev(name: &str, tool: &str, id: &str, detail: &str) -> Event {
    Event {
        tool: tool.into(),
        tool_id: id.into(),
        detail: detail.into(),
        ..ev(name)
    }
}

fn facts(p: Pane) -> Facts {
    Facts {
        codex: None,
        now: NOW,
        host: "box".into(),
        pane: p,
        vis: Some(Visibility::Away),
        others: Vec::new(),
        bg_shells: 0,
        remind_after: String::new(),
        test_regex: String::new(),
    }
}

struct Run(Vec<Effect>);

impl Run {
    fn ops(&self) -> &[Op] {
        match self.0.first() {
            Some(Effect::Options(ops)) => ops,
            _ => &[],
        }
    }
    /// The option after the writes: `Some(Some(v))` set, `Some(None)`
    /// unset, `None` untouched.
    fn opt(&self, name: &str) -> Option<Option<&str>> {
        self.ops().iter().rev().find_map(|op| match op {
            Op::Set(n, v) if *n == name => Some(Some(v.as_str())),
            Op::Unset(n) if *n == name => Some(None),
            _ => None,
        })
    }
    fn state(&self) -> Option<&str> {
        self.opt("@agent_state").flatten()
    }
    fn sounds(&self) -> Vec<&'static str> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Effect::Sound(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
    fn notifies(&self) -> Vec<(Urgency, &str, &str)> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Effect::Notify {
                    urgency,
                    title,
                    body,
                } => Some((*urgency, title.as_str(), body.as_str())),
                _ => None,
            })
            .collect()
    }
    fn has(&self, effect: &Effect) -> bool {
        self.0.contains(effect)
    }
    fn arms(&self) -> Vec<(f64, i64)> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Effect::RemindArm { after, since } => Some((*after, *since)),
                _ => None,
            })
            .collect()
    }
}

fn run_with(state: &mut State, event: Event, f: Facts) -> Run {
    run_in(state, "%1", event, f)
}

fn run_in(state: &mut State, pane_id: &str, event: Event, f: Facts) -> Run {
    let input = Input {
        kind: Kind::Claude,
        pane: pane_id.into(),
        event,
        config_dir: None,
        agent_pid: 4242,
        observing: false,
    };
    Run(handle(state, &input, &f))
}

fn run(event: Event, p: Pane) -> Run {
    run_with(&mut State::default(), event, facts(p))
}

fn run_vis(event: Event, p: Pane, vis: Visibility) -> Run {
    let mut f = facts(p);
    f.vis = Some(vis);
    run_with(&mut State::default(), event, f)
}

fn with_subagent(sid: &str, id: &str, kind: &str) -> State {
    let mut s = State::default();
    s.subagents
        .entry(sid.into())
        .or_default()
        .insert(id.into(), kind.into());
    s
}

// --- §3 Claude events → state ------------------------------------------------

#[test]
fn h1_session_start_is_ready_and_forgets_subagents() {
    let mut state = with_subagent("old", "a1", "Explore");
    state
        .subagents
        .entry("s1".into())
        .or_default()
        .insert("a2".into(), "Plan".into());
    let p = Pane {
        sid: "old".into(),
        ..in_state("idle")
    };
    let r = run_with(&mut state, ev("SessionStart"), facts(p));
    assert_eq!(r.state(), Some("ready"));
    assert_eq!(r.opt("@agent_since"), Some(Some(NOW.to_string().as_str())));
    assert_eq!(r.opt("@agent_subs"), Some(Some("0")));
    assert_eq!(r.opt("@agent_subtypes"), Some(None));
    assert_eq!(r.opt("@agent_tool"), Some(None));
    assert_eq!(r.opt("@agent_msg"), Some(None));
    assert!(state.subagents.is_empty());
}

#[test]
fn h1b_compact_session_start_keeps_the_state() {
    let mut state = with_subagent("s1", "a1", "Explore");
    let e = Event {
        source: "compact".into(),
        ..ev("SessionStart")
    };
    let r = run_with(&mut state, e, facts(in_state("compacting")));
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(r.opt("@agent_subs"), None);
    assert_eq!(r.opt("@agent"), Some(Some("claude")));
    assert_eq!(state.subagents.len(), 1);
}

#[test]
fn h1c_model_on_any_session_start() {
    for source in ["startup", "compact"] {
        let e = Event {
            source: source.into(),
            model: "opus".into(),
            ..ev("SessionStart")
        };
        assert_eq!(run(e, pane()).opt("@agent_model"), Some(Some("opus")));
    }
    assert_eq!(run(ev("SessionStart"), pane()).opt("@agent_model"), None);
}

#[test]
fn h2_prompt_is_working_and_joins_the_round() {
    let mut state = State::default();
    let r = run_with(&mut state, ev("UserPromptSubmit"), facts(in_state("idle")));
    assert_eq!(r.state(), Some("working"));
    assert_eq!(r.opt("@agent_tool"), Some(None));
    assert_eq!(r.opt("@agent_msg"), Some(None));
    assert!(state.rounds["work"].contains("%1"));
}

#[test]
fn h3_pre_tool_use() {
    let r = run(
        tool_ev("PreToolUse", "Bash", "t1", "echo #1"),
        in_state("idle"),
    );
    assert_eq!(r.state(), Some("working"));
    assert_eq!(r.opt("@agent_tool"), Some(Some("Bash: echo ##1")));
    let r = run(tool_ev("PreToolUse", "Read", "t2", ""), in_state("needs"));
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(r.opt("@agent_tool"), Some(Some("Read")));
}

#[test]
fn h4_permission_request_kinds() {
    for (tool, kind) in [
        ("AskUserQuestion", "question"),
        ("ExitPlanMode", "plan"),
        ("Bash", "permission"),
    ] {
        let r = run(
            tool_ev("PermissionRequest", tool, "t9", "x"),
            in_state("working"),
        );
        assert_eq!(r.state(), Some("needs"));
        assert_eq!(r.opt("@agent_needs"), Some(Some(kind)));
        assert_eq!(r.opt("@agent_needs_id"), Some(Some("t9")));
        assert_eq!(
            r.opt("@agent_tool"),
            Some(Some(format!("{tool}: x").as_str()))
        );
    }
    // An empty tool id is still written (bash sets it to "").
    let r = run(
        tool_ev("PermissionRequest", "Bash", "", ""),
        in_state("working"),
    );
    assert_eq!(r.opt("@agent_needs_id"), Some(Some("")));
}

#[test]
fn h5_only_the_waiting_call_ends_the_wait() {
    for ev_name in ["PostToolUse", "PostToolUseFailure"] {
        let waiting = Pane {
            needs_id: "a".into(),
            ..in_state("needs")
        };
        let r = run(tool_ev(ev_name, "Bash", "b", ""), waiting.clone());
        assert_eq!(r.opt("@agent_state"), None, "{ev_name}: other call");
        let r = run(tool_ev(ev_name, "Bash", "a", ""), waiting);
        assert_eq!(r.state(), Some("working"));
        assert_eq!(r.opt("@agent_needs"), Some(None));
        assert_eq!(r.opt("@agent_needs_id"), Some(None));
        let no_id = in_state("needs");
        assert_eq!(
            run(tool_ev(ev_name, "Bash", "b", ""), no_id).state(),
            Some("working")
        );
        assert_eq!(
            run(tool_ev(ev_name, "Bash", "b", ""), in_state("idle")).state(),
            Some("working")
        );
    }
}

fn notification(ntype: &str) -> Event {
    Event {
        ntype: ntype.into(),
        ..ev("Notification")
    }
}

#[test]
fn h6_permission_prompt() {
    let r = run(notification("permission_prompt"), in_state("working"));
    assert_eq!(r.state(), Some("needs"));
    assert_eq!(r.opt("@agent_needs"), Some(Some("permission")));
    // Already waiting: unchanged, but still a handled event (H14).
    let r = run(notification("permission_prompt"), in_state("needs"));
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(r.opt("@agent"), Some(Some("claude")));
}

#[test]
fn h6b_questions() {
    for t in [
        "elicitation_dialog",
        "elicitation_url_dialog",
        "agent_needs_input",
    ] {
        let r = run(notification(t), in_state("needs"));
        assert_eq!(r.state(), Some("needs"));
        assert_eq!(r.opt("@agent_needs"), Some(Some("question")));
    }
}

#[test]
fn h6c_other_notifications_do_nothing() {
    assert!(
        run(notification("idle_prompt"), in_state("working"))
            .0
            .is_empty()
    );
    assert!(run(notification(""), in_state("working")).0.is_empty());
}

#[test]
fn h7_elicitation() {
    let r = run(ev("Elicitation"), in_state("working"));
    assert_eq!(
        (r.state(), r.opt("@agent_needs")),
        (Some("needs"), Some(Some("question")))
    );
    assert_eq!(
        run(ev("ElicitationResult"), in_state("needs")).state(),
        Some("working")
    );
    assert_eq!(
        run(ev("ElicitationResult"), in_state("idle")).opt("@agent_state"),
        None
    );
}

#[test]
fn h8_compaction() {
    let r = run(ev("PreCompact"), in_state("working"));
    assert_eq!(r.state(), Some("compacting"));
    assert_eq!(r.opt("@agent_prev"), Some(Some("working")));
    let r = run(ev("PreCompact"), in_state("compacting"));
    assert_eq!(r.opt("@agent_prev"), None);
    let r = run(ev("PreCompact"), pane());
    assert_eq!(r.opt("@agent_prev"), Some(Some("")));
}

#[test]
fn h8b_post_compact_restores() {
    let with_prev = |prev: &str| Pane {
        prev: prev.into(),
        ..in_state("compacting")
    };
    assert_eq!(
        run(ev("PostCompact"), with_prev("working")).state(),
        Some("working")
    );
    assert_eq!(run(ev("PostCompact"), with_prev("")).state(), Some("idle"));
    assert_eq!(
        run(ev("PostCompact"), with_prev("compacting")).state(),
        Some("idle")
    );
    assert_eq!(
        run(ev("PostCompact"), with_prev("ready")).state(),
        Some("ready")
    );
    // Whatever @agent_prev holds is restored.
    assert_eq!(
        run(ev("PostCompact"), with_prev("odd")).state(),
        Some("odd")
    );
}

#[test]
fn h9_stop() {
    let e = Event {
        last: "All #done".into(),
        ..ev("Stop")
    };
    let r = run(e.clone(), in_state("working"));
    assert_eq!(r.state(), Some("done"));
    assert_eq!(r.opt("@agent_msg"), Some(Some("All ##done")));
    assert_eq!(r.opt("@agent_tool"), Some(None));
    let r = run_vis(e, in_state("working"), Visibility::Visible);
    assert_eq!(r.state(), Some("idle"));
    // An empty reply is still written.
    assert_eq!(
        run(ev("Stop"), in_state("working")).opt("@agent_msg"),
        Some(Some(""))
    );
}

#[test]
fn h10_stop_failure_message() {
    let e = |error: &str, last: &str| Event {
        error: error.into(),
        last: last.into(),
        ..ev("StopFailure")
    };
    let r = run(e("rate #limit", "x"), in_state("working"));
    assert_eq!(r.state(), Some("error"));
    assert_eq!(r.opt("@agent_msg"), Some(Some("rate ##limit")));
    assert_eq!(
        run(e("", "last"), pane()).opt("@agent_msg"),
        Some(Some("last"))
    );
    assert_eq!(
        run(e("", ""), pane()).opt("@agent_msg"),
        Some(Some("unknown error"))
    );
}

#[test]
fn h11_session_end_clears_everything() {
    let mut state = with_subagent("s1", "a1", "Explore");
    let r = run_with(&mut state, ev("SessionEnd"), facts(in_state("needs")));
    for name in ALL_OPTIONS {
        assert_eq!(r.opt(name), Some(None), "{name}");
    }
    assert!(r.ops().iter().all(|op| matches!(op, Op::Unset(_))));
    assert!(r.has(&Effect::NotifyClose));
    assert!(r.has(&Effect::RemindCancel));
    assert!(r.sounds().is_empty() && r.notifies().is_empty());
    assert!(state.subagents.is_empty());
}

#[test]
fn h12_other_events_do_nothing() {
    for name in [
        "Interrupt",
        "CodexReconcile",
        "SubagentStart",
        "Whatever",
        "",
    ] {
        assert!(run(ev(name), in_state("working")).0.is_empty(), "{name}");
    }
}

#[test]
fn h13_since_only_on_change() {
    let r = run(ev("UserPromptSubmit"), in_state("working"));
    assert_eq!(r.state(), Some("working"));
    assert_eq!(r.opt("@agent_since"), None);
    // Stop while visible from idle: idle again, since unchanged.
    let r = run_vis(ev("Stop"), in_state("idle"), Visibility::Visible);
    assert_eq!(r.state(), Some("idle"));
    assert_eq!(r.opt("@agent_since"), None);
    let r = run(ev("UserPromptSubmit"), in_state("idle"));
    assert_eq!(r.opt("@agent_since"), Some(Some(NOW.to_string().as_str())));
}

#[test]
fn h13_needs_kind_only_when_given() {
    // PostCompact back to needs has no kind: @agent_needs is left alone.
    let p = Pane {
        prev: "needs".into(),
        ..in_state("compacting")
    };
    let r = run(ev("PostCompact"), p);
    assert_eq!(r.state(), Some("needs"));
    assert_eq!(r.opt("@agent_needs"), None);
    assert_eq!(r.opt("@agent_needs_id"), None);
}

#[test]
fn h14_identity() {
    let e = Event {
        transcript: "/t.jsonl".into(),
        mode: "plan".into(),
        ..ev("UserPromptSubmit")
    };
    let r = run(e, pane());
    assert_eq!(r.opt("@agent"), Some(Some("claude")));
    assert_eq!(r.opt("@agent_session"), Some(Some("s1")));
    assert_eq!(r.opt("@agent_transcript"), Some(Some("/t.jsonl")));
    assert_eq!(r.opt("@agent_mode"), Some(Some("plan")));
    assert_eq!(r.opt("@agent_profile"), Some(Some("default")));
    let e = Event {
        sid: String::new(),
        ..ev("UserPromptSubmit")
    };
    let r = run(e, pane());
    assert_eq!(r.opt("@agent_session"), None);
    assert_eq!(r.opt("@agent_transcript"), None);
    assert_eq!(r.opt("@agent_mode"), None);
}

#[test]
fn h14_profile() {
    let profile = |dir: Option<&str>| {
        let input = Input {
            kind: Kind::Claude,
            pane: "%1".into(),
            event: ev("UserPromptSubmit"),
            config_dir: dir.map(String::from),
            agent_pid: 1,
            observing: false,
        };
        let r = Run(handle(&mut State::default(), &input, &facts(pane())));
        r.opt("@agent_profile").flatten().map(String::from)
    };
    assert_eq!(profile(None).as_deref(), Some("default"));
    assert_eq!(profile(Some("")).as_deref(), Some("default"));
    assert_eq!(profile(Some("/h/.claude")).as_deref(), Some("default"));
    assert_eq!(profile(Some("/h/.claude/")).as_deref(), Some("default"));
    assert_eq!(
        profile(Some("/h/.config/claude/work/")).as_deref(),
        Some("work")
    );
    assert_eq!(profile(Some("work")).as_deref(), Some("work"));
}

// --- §4 order ----------------------------------------------------------------

#[test]
fn o1_options_come_first() {
    let r = run(
        tool_ev("PermissionRequest", "Bash", "t", "ls"),
        in_state("working"),
    );
    assert!(matches!(r.0.first(), Some(Effect::Options(_))));
    assert!(r.0[1..].iter().all(|e| !matches!(e, Effect::Options(_))));
}

// --- §5 subagents --------------------------------------------------------------

fn sub(name: &str, id: &str, kind: &str) -> Event {
    Event {
        agent_id: id.into(),
        agent_type: kind.into(),
        ..ev(name)
    }
}

#[test]
fn a1_subagent_events_only_count() {
    let mut state = State::default();
    let r = run_with(
        &mut state,
        sub("SubagentStart", "a1", "Explore"),
        facts(in_state("working")),
    );
    assert_eq!(r.0.len(), 1);
    assert_eq!(r.opt("@agent_subs"), Some(Some("1")));
    assert_eq!(r.opt("@agent_subtypes"), Some(Some("1 Explore")));
    assert_eq!(r.opt("@agent_state"), None);
    assert_eq!(r.opt("@agent"), None);
    // Any other event of a subagent is ignored entirely.
    let r = run_with(
        &mut state,
        sub("Stop", "a1", ""),
        facts(in_state("working")),
    );
    assert!(r.0.is_empty());
    let r = run_with(
        &mut state,
        sub("SubagentStop", "a1", ""),
        facts(in_state("working")),
    );
    assert_eq!(r.opt("@agent_subs"), Some(Some("0")));
    assert_eq!(r.opt("@agent_subtypes"), Some(None));
    assert!(state.subagents.is_empty());
}

#[test]
fn a2_subtypes() {
    let mut state = State::default();
    let f = || facts(in_state("working"));
    run_with(&mut state, sub("SubagentStart", "a1", "Plan"), f());
    run_with(&mut state, sub("SubagentStart", "a2", "Explore"), f());
    run_with(&mut state, sub("SubagentStart", "a3", "default"), f());
    let r = run_with(&mut state, sub("SubagentStart", "a4", "Explore"), f());
    assert_eq!(r.opt("@agent_subs"), Some(Some("4")));
    assert_eq!(
        r.opt("@agent_subtypes"),
        Some(Some("1 agent, 2 Explore, 1 Plan"))
    );
    // Stopping an unknown subagent still publishes the count.
    let r = run_with(&mut state, sub("SubagentStop", "zz", ""), f());
    assert_eq!(r.opt("@agent_subs"), Some(Some("4")));
}

#[test]
fn a3_per_session() {
    let mut state = with_subagent("other", "a1", "Plan");
    let r = run_with(
        &mut state,
        sub("SubagentStart", "a2", "Explore"),
        facts(pane()),
    );
    assert_eq!(r.opt("@agent_subs"), Some(Some("1")));
    run_with(&mut state, ev("SessionEnd"), facts(pane()));
    assert!(state.subagents.contains_key("other"));
    assert!(!state.subagents.contains_key("s1"));
}

#[test]
fn a4_done_with_subagents_is_quiet() {
    let mut state = with_subagent("s1", "a1", "Explore");
    let r = run_with(&mut state, ev("Stop"), facts(in_state("working")));
    assert_eq!(r.state(), Some("done"));
    assert!(r.sounds().is_empty());
    assert!(r.notifies().is_empty());
    assert!(r.has(&Effect::Blink));
}

// --- §8 sounds -------------------------------------------------------------

#[test]
fn s1_entering_needs_sound_by_kind() {
    for (tool, sound) in [
        ("AskUserQuestion", "report-in"),
        ("ExitPlanMode", "wait-for-my-go"),
        ("Bash", "need-backup"),
    ] {
        let e = tool_ev("PermissionRequest", tool, "t", "");
        assert_eq!(run(e.clone(), in_state("working")).sounds(), vec![sound]);
        assert_eq!(
            run_vis(e.clone(), in_state("working"), Visibility::Session).sounds(),
            vec![sound]
        );
        assert!(
            run_vis(e.clone(), in_state("working"), Visibility::Visible)
                .sounds()
                .is_empty()
        );
        // Already waiting: not entering.
        assert!(run(e, in_state("needs")).sounds().is_empty());
    }
    assert_eq!(
        run(ev("Elicitation"), in_state("working")).sounds(),
        vec!["report-in"]
    );
}

#[test]
fn s1_needs_without_kind_is_need_backup() {
    let p = Pane {
        prev: "needs".into(),
        ..in_state("compacting")
    };
    assert_eq!(run(ev("PostCompact"), p).sounds(), vec!["need-backup"]);
}

fn other(pane: &str, state: &str, subs: &str, space: &str) -> OtherPane {
    OtherPane {
        pane: pane.into(),
        state: state.into(),
        subs: subs.into(),
        space: space.into(),
    }
}

#[test]
fn s2_r_round_won_by_the_last_of_two() {
    let mut state = State::default();
    let f = |others: Vec<OtherPane>, vis: Visibility| {
        let mut f = facts(in_state("working"));
        f.others = others;
        f.vis = Some(vis);
        f
    };
    run_in(
        &mut state,
        "%1",
        ev("UserPromptSubmit"),
        facts(in_state("idle")),
    );
    run_in(
        &mut state,
        "%2",
        ev("UserPromptSubmit"),
        facts(in_state("idle")),
    );
    // %1 stops while %2 works: the round goes on, %1 gets the plain sound.
    let others = vec![
        other("%1", "working", "", "work"),
        other("%2", "working", "", "work"),
    ];
    let r = run_in(&mut state, "%1", ev("Stop"), f(others, Visibility::Away));
    assert_eq!(r.sounds(), vec!["enemy-down"]);
    assert_eq!(state.rounds["work"].len(), 2);
    // %2 stops last: won, even when visible.
    let others = vec![
        other("%1", "done", "", "work"),
        other("%2", "working", "", "work"),
    ];
    let r = run_in(&mut state, "%2", ev("Stop"), f(others, Visibility::Visible));
    assert_eq!(r.sounds(), vec!["ct-win"]);
    assert!(!state.rounds.contains_key("work"));
}

#[test]
fn r_one_pane_is_not_a_win_and_ends_the_round() {
    let mut state = State::default();
    run_with(&mut state, ev("UserPromptSubmit"), facts(in_state("idle")));
    run_with(
        &mut state,
        ev("UserPromptSubmit"),
        facts(in_state("working")),
    );
    let r = run_with(&mut state, ev("Stop"), facts(in_state("working")));
    assert_eq!(r.sounds(), vec!["enemy-down"]);
    assert!(state.rounds.is_empty());
}

#[test]
fn r_busy_means_working_needs_compacting_or_subagents_in_the_same_space() {
    let busy = [
        other("%2", "needs", "", "work"),
        other("%2", "compacting", "", "work"),
        other("%2", "done", "2", "work"),
    ];
    let idle = [
        other("%2", "working", "", "home"),
        other("%2", "done", "0", "work"),
        other("%2", "idle", "", "work"),
    ];
    for (others, won) in [(&busy[..], false), (&idle[..], true)] {
        for o in others {
            let mut state = State::default();
            state
                .rounds
                .insert("work".into(), ["%1".into(), "%3".into()].into());
            let mut f = facts(in_state("working"));
            f.others = vec![o.clone()];
            let r = run_with(&mut state, ev("Stop"), f);
            assert_eq!(r.sounds() == vec!["ct-win"], won, "{o:?}");
        }
    }
}

#[test]
fn s3_enemy_down_not_when_visible() {
    assert_eq!(
        run(ev("Stop"), in_state("working")).sounds(),
        vec!["enemy-down"]
    );
    assert_eq!(
        run_vis(ev("Stop"), in_state("working"), Visibility::Session).sounds(),
        vec!["enemy-down"]
    );
    assert!(
        run_vis(ev("Stop"), in_state("working"), Visibility::Visible)
            .sounds()
            .is_empty()
    );
}

#[test]
fn s4_oh_man_any_visibility() {
    for vis in [Visibility::Away, Visibility::Session, Visibility::Visible] {
        assert_eq!(
            run_vis(ev("StopFailure"), in_state("working"), vis).sounds(),
            vec!["oh-man"]
        );
    }
}

#[test]
fn s5_accepted_plan() {
    let e = tool_ev("PostToolUse", "ExitPlanMode", "t", "");
    assert_eq!(run(e, in_state("needs")).sounds(), vec!["lets-do-this"]);
    let e = tool_ev("PostToolUseFailure", "ExitPlanMode", "t", "");
    assert!(run(e, in_state("needs")).sounds().is_empty());
}

#[test]
fn s6_test_run_sound_every_ten_minutes() {
    let bash = |cmd: &str| tool_ev("PreToolUse", "Bash", "t", cmd);
    let r = run(bash("cd x && cargo test --all"), in_state("working"));
    assert_eq!(r.sounds(), vec!["fight-like-a-man"]);
    assert_eq!(
        r.opt("@agent_tests_sound_at"),
        Some(Some(NOW.to_string().as_str()))
    );
    for cmd in [
        "pytest -q",
        "uv run pytest",
        "npm run test",
        "go test ./...",
        "(make check)",
        "python3 -m unittest",
    ] {
        assert_eq!(
            run(bash(cmd), pane()).sounds(),
            vec!["fight-like-a-man"],
            "{cmd}"
        );
    }
    for cmd in ["pytestify", "echo cargo testing", "cargo build"] {
        assert!(run(bash(cmd), pane()).sounds().is_empty(), "{cmd}");
    }
    // Not a Bash call.
    assert!(
        run(tool_ev("PreToolUse", "Read", "t", "pytest"), pane())
            .sounds()
            .is_empty()
    );
    // Within 600 s of the last one.
    let recent = Pane {
        tests_sound_at: (NOW - 599).to_string(),
        ..pane()
    };
    let r = run(bash("pytest"), recent);
    assert!(r.sounds().is_empty());
    assert_eq!(r.opt("@agent_tests_sound_at"), None);
    let old = Pane {
        tests_sound_at: (NOW - 600).to_string(),
        ..pane()
    };
    assert_eq!(run(bash("pytest"), old).sounds(), vec!["fight-like-a-man"]);
}

#[test]
fn s6_muted_still_records_the_time() {
    let p = Pane {
        muted: true,
        ..pane()
    };
    let r = run(tool_ev("PreToolUse", "Bash", "t", "pytest"), p);
    assert!(r.sounds().is_empty());
    assert_eq!(
        r.opt("@agent_tests_sound_at"),
        Some(Some(NOW.to_string().as_str()))
    );
}

#[test]
fn s8_custom_regex() {
    let mut f = facts(pane());
    f.test_regex = "^just check$".into();
    let r = run_with(
        &mut State::default(),
        tool_ev("PreToolUse", "Bash", "t", "just check"),
        f.clone(),
    );
    assert_eq!(r.sounds(), vec!["fight-like-a-man"]);
    let r = run_with(
        &mut State::default(),
        tool_ev("PreToolUse", "Bash", "t", "pytest"),
        f.clone(),
    );
    assert!(r.sounds().is_empty());
    f.test_regex = "(".into(); // invalid: matches nothing
    let r = run_with(
        &mut State::default(),
        tool_ev("PreToolUse", "Bash", "t", "("),
        f,
    );
    assert!(r.sounds().is_empty());
}

#[test]
fn sounds_are_skipped_in_a_muted_space() {
    let muted = Pane {
        muted: true,
        ..in_state("working")
    };
    for e in [
        tool_ev("PermissionRequest", "Bash", "t", ""),
        ev("Stop"),
        ev("StopFailure"),
        tool_ev("PostToolUse", "ExitPlanMode", "t", ""),
    ] {
        let r = run(e.clone(), muted.clone());
        assert!(r.sounds().is_empty(), "{}", e.ev);
        assert!(r.notifies().is_empty(), "{}", e.ev);
    }
}

// --- §11 notifications and reminders ------------------------------------------

#[test]
fn n1_only_when_away() {
    let e = tool_ev("PermissionRequest", "Bash", "t", "ls");
    assert_eq!(run(e.clone(), in_state("working")).notifies().len(), 1);
    assert!(
        run_vis(e.clone(), in_state("working"), Visibility::Session)
            .notifies()
            .is_empty()
    );
    assert!(
        run_vis(e, in_state("working"), Visibility::Visible)
            .notifies()
            .is_empty()
    );
    // Not on needs -> needs.
    let e = tool_ev("PermissionRequest", "Bash", "t2", "ls");
    assert!(run(e, in_state("needs")).notifies().is_empty());
}

#[test]
fn n2_needs_bodies() {
    let body = |e: Event, p: Pane| run(e, p).notifies()[0].2.to_string();
    let e = tool_ev("PermissionRequest", "Bash", "t", "echo #1");
    assert_eq!(
        body(e, in_state("working")),
        "Needs permission: Bash: echo #1"
    );
    let e = tool_ev("PermissionRequest", "AskUserQuestion", "t", "x");
    assert_eq!(body(e, in_state("working")), "Has a question for you");
    let e = tool_ev("PermissionRequest", "ExitPlanMode", "t", "x");
    assert_eq!(body(e, in_state("working")), "Plan ready for your review");
    // Notification: the pane's last tool, unescaped.
    let p = Pane {
        tool: "Bash: echo ##2".into(),
        ..in_state("working")
    };
    assert_eq!(
        body(notification("permission_prompt"), p),
        "Needs permission: Bash: echo #2"
    );
    assert_eq!(
        body(notification("permission_prompt"), in_state("working")),
        "Needs permission"
    );
    let r = run(
        tool_ev("PermissionRequest", "Bash", "t", "ls"),
        in_state("working"),
    );
    assert_eq!(r.notifies()[0].0, Urgency::Critical);
    assert_eq!(r.notifies()[0].1, "api · Fix CSV");
}

#[test]
fn n2_done_and_error_bodies() {
    let e = Event {
        last: "All set".into(),
        ..ev("Stop")
    };
    let r = run(e.clone(), in_state("working"));
    assert_eq!(
        r.notifies(),
        vec![(Urgency::Normal, "api · Fix CSV", "All set")]
    );
    assert_eq!(
        run(ev("Stop"), in_state("working")).notifies()[0].2,
        "Finished"
    );
    for (n, suffix) in [
        (1, " · 1 shell still running"),
        (3, " · 3 shells still running"),
    ] {
        let mut f = facts(in_state("working"));
        f.bg_shells = n;
        let r = run_with(&mut State::default(), e.clone(), f);
        assert_eq!(r.notifies()[0].2, format!("All set{suffix}"));
        assert!(r.has(&Effect::Bgwatch { agent_pid: 4242 }));
        assert_eq!(r.opt("@agent_bg"), Some(Some(n.to_string().as_str())));
    }
    let e = Event {
        error: "rate limit".into(),
        ..ev("StopFailure")
    };
    assert_eq!(
        run(e, in_state("working")).notifies(),
        vec![(Urgency::Critical, "api · Fix CSV", "Stopped: rate limit")]
    );
    assert_eq!(
        run(ev("StopFailure"), in_state("working")).notifies()[0].2,
        "Stopped: error"
    );
}

#[test]
fn n2_title_without_host_task() {
    let p = Pane {
        title: "box".into(),
        ..in_state("working")
    };
    assert_eq!(run(ev("Stop"), p).notifies()[0].1, "api");
}

#[test]
fn n3_leaving_needs_closes_and_cancels() {
    let r = run(tool_ev("PostToolUse", "Bash", "t", ""), in_state("needs"));
    assert_eq!(r.0[1], Effect::NotifyClose);
    assert_eq!(r.0[2], Effect::RemindCancel);
    // From needs to done: close the old one before showing the new one.
    let r = run(ev("Stop"), in_state("needs"));
    let close = r.0.iter().position(|e| *e == Effect::NotifyClose).unwrap();
    let show =
        r.0.iter()
            .position(|e| matches!(e, Effect::Notify { .. }))
            .unwrap();
    assert!(close < show);
    // Staying in needs, or not being in needs: nothing to close.
    let r = run(
        tool_ev("PermissionRequest", "Bash", "t", ""),
        in_state("needs"),
    );
    assert!(!r.has(&Effect::NotifyClose));
    let r = run(ev("UserPromptSubmit"), in_state("done"));
    assert!(!r.has(&Effect::NotifyClose));
}

#[test]
fn n4_reminder_armed_on_entering_needs() {
    let e = tool_ev("PermissionRequest", "Bash", "t", "");
    assert_eq!(
        run(e.clone(), in_state("working")).arms(),
        vec![(900.0, NOW)]
    );
    // Also when visible or muted: the check happens when it fires.
    assert_eq!(
        run_vis(e.clone(), in_state("working"), Visibility::Visible)
            .arms()
            .len(),
        1
    );
    let muted = Pane {
        muted: true,
        ..in_state("working")
    };
    assert_eq!(run(e.clone(), muted).arms().len(), 1);
    assert!(run(e.clone(), in_state("needs")).arms().is_empty());
    let mut f = facts(in_state("working"));
    for (raw, after) in [
        ("3", 3.0),
        ("0.5", 0.5),
        ("2m", 120.0),
        ("1h", 3600.0),
        ("0", 0.0),
        ("", 900.0),
        // C7: unreadable is the default, not "at once" as in bash.
        ("-1", 900.0),
        ("x", 900.0),
        ("inf", 900.0),
        ("m", 900.0),
    ] {
        f.remind_after = raw.into();
        let r = run_with(&mut State::default(), e.clone(), f.clone());
        assert_eq!(r.arms(), vec![(after, NOW)], "{raw}");
    }
}

#[test]
fn c2_reminder_since_is_the_written_since() {
    let r = run(ev("Elicitation"), in_state("working"));
    let since = r.opt("@agent_since").flatten().unwrap().to_string();
    assert_eq!(r.arms()[0].1.to_string(), since);
}

#[test]
fn n4_reminder_fires() {
    let since = NOW - 900;
    let waiting = Pane {
        state: "needs".into(),
        since: since.to_string(),
        ..pane()
    };
    let r = Run(reminder(since, &facts(waiting.clone())));
    assert_eq!(r.sounds(), vec!["come-to-papa"]);
    assert_eq!(
        r.notifies(),
        vec![(
            Urgency::Critical,
            "api · Fix CSV",
            "Still waiting for you, 15 min now"
        )]
    );
    let mut f = facts(waiting.clone());
    f.vis = Some(Visibility::Session);
    assert_eq!(Run(reminder(since, &f)).sounds().len(), 1);
    f.vis = Some(Visibility::Visible);
    assert!(reminder(since, &f).is_empty());
    // Another needs since, left needs, or muted: nothing.
    assert!(reminder(since - 1, &facts(waiting.clone())).is_empty());
    let left = Pane {
        state: "working".into(),
        ..waiting.clone()
    };
    assert!(reminder(since, &facts(left)).is_empty());
    let muted = Pane {
        muted: true,
        ..waiting
    };
    assert!(reminder(since, &facts(muted)).is_empty());
}

// --- §12 seen ---------------------------------------------------------------

#[test]
fn e1_seen() {
    let r = Run(seen(&in_state("done")));
    assert_eq!(r.state(), Some("idle"));
    assert_eq!(r.opt("@agent_since"), None);
    assert!(r.has(&Effect::NotifyClose));
    for state in ["needs", "working", "idle", ""] {
        let r = Run(seen(&in_state(state)));
        assert_eq!(r.opt("@agent_state"), None, "{state}");
        assert!(r.has(&Effect::NotifyClose), "{state}");
    }
}

// --- §10 blink ---------------------------------------------------------------

#[test]
fn k5_blink_starts_on_needs_and_done() {
    assert!(run(ev("Elicitation"), in_state("working")).has(&Effect::Blink));
    assert!(run(ev("Stop"), in_state("working")).has(&Effect::Blink));
    assert!(!run_vis(ev("Stop"), in_state("working"), Visibility::Visible).has(&Effect::Blink));
    assert!(!run(ev("UserPromptSubmit"), in_state("idle")).has(&Effect::Blink));
}

// --- what the daemon must read ---------------------------------------------

/// Events outside `may_need_visibility` behave the same whatever the
/// visibility, and those outside `may_need_panes` whatever the other panes.
#[test]
fn v1_facts_predicates_are_supersets() {
    let events = [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "Elicitation",
        "ElicitationResult",
        "PreCompact",
        "PostCompact",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ];
    let states = ["", "working", "needs", "compacting", "done", "idle"];
    let busy = vec![other("%2", "working", "", "work")];
    for name in events {
        for cur in states {
            for prev in ["", "needs", "done", "error"] {
                let p = Pane {
                    prev: prev.into(),
                    ..in_state(cur)
                };
                let e = Event {
                    ntype: "permission_prompt".into(),
                    ..tool_ev(name, "Bash", "t", "ls")
                };
                let with = |vis: Visibility, others: Vec<OtherPane>| {
                    let mut st = State::default();
                    st.rounds
                        .insert("work".into(), ["%1".into(), "%3".into()].into());
                    let mut f = facts(p.clone());
                    f.vis = Some(vis);
                    f.others = others;
                    run_with(&mut st, e.clone(), f).0
                };
                if !may_need_visibility(Kind::Claude, name) {
                    assert_eq!(
                        with(Visibility::Visible, vec![]),
                        with(Visibility::Away, vec![]),
                        "{name} {cur}"
                    );
                }
                if !may_need_panes(Kind::Claude, name) {
                    assert_eq!(
                        with(Visibility::Away, busy.clone()),
                        with(Visibility::Away, vec![]),
                        "{name} {cur}"
                    );
                }
            }
        }
    }
}

// --- §1 ownership --------------------------------------------------------------

fn chain<'a>(names: &[&'a str]) -> Vec<(u32, &'a str)> {
    // pids 100, 101, ... from the hook's parent upwards
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (100 + i as u32, *n))
        .collect()
}

#[test]
fn i4_the_panes_own_agent() {
    // hook <- claude <- shell (pane_pid)
    assert_eq!(owner(chain(&["claude", "zsh"]), 101), Some(100));
    // The agent is the pane's process itself.
    assert_eq!(owner(chain(&["node", "claude"]), 101), Some(101));
    // A `claude -p` run by the pane's agent: two agents on the way.
    assert_eq!(
        owner(chain(&["claude", "bash", "claude", "zsh"]), 103),
        None
    );
    // No agent, or the walk never reaches the pane.
    assert_eq!(owner(chain(&["bash", "zsh"]), 101), None);
    assert_eq!(owner(chain(&["claude", "zsh"]), 999), None);
    // codex counts the same.
    assert_eq!(owner(chain(&["codex", "zsh"]), 101), Some(100));
    // Names are exact.
    assert_eq!(owner(chain(&["claude-code", "zsh"]), 101), None);
}

#[test]
fn c4_thirteen_processes_all_counted() {
    let mut names = vec!["sh"; 12];
    names[0] = "claude";
    names.push("zsh"); // pane_pid is the 13th
    assert_eq!(owner(chain(&names), 112), Some(100));
    // The 13th is the only agent: counted like the others (bash rejects).
    let mut names = vec!["sh"; 12];
    names.push("claude");
    assert_eq!(owner(chain(&names), 112), Some(112));
    // Another agent among the first 12 and the 13th: two (bash accepts).
    let mut names = vec!["sh"; 12];
    names[0] = "claude";
    names.push("codex");
    assert_eq!(owner(chain(&names), 112), None);
    // pane_pid 14th: too far.
    let mut names = vec!["sh"; 13];
    names[0] = "claude";
    names.push("zsh");
    assert_eq!(owner(chain(&names), 113), None);
}
