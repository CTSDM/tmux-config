//! Effects. Sounds play in-process; the bash helpers of `@agents_bin` still
//! show notifications, watch background shells and blink until their tasks
//! of phase 3 and 4 move them in (design.md, "Migration"). Reminders are the
//! daemon's own timers (mod.rs).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::sound;
use crate::core::Effect;

/// What effects need from the whole daemon.
#[derive(Debug, Clone)]
pub struct Env {
    pub sound: sound::Config,
}

/// `agent-notify --close` is a uv script: give it time to start.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the effects of one event need besides themselves.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub pane: String,
    /// `@agents_bin`, read with the event.
    pub bin: String,
    /// The pane had a notification open (`@agent_notify_id`) when read.
    pub notify_open: bool,
    /// `$TMUX` for the helpers.
    pub tmux_env: String,
    /// `@agent_sound` is not `off`, and `@agent_sound_volume`.
    pub sound_on: bool,
    pub volume: String,
}

/// Runs one effect. A close waits for its helper, so a notification shown
/// afterwards can't be closed by it (O3).
pub async fn run(env: &Env, ctx: &Ctx, effect: &Effect) {
    if let Effect::Sound(name) = effect {
        sound::play(&env.sound, name, ctx.sound_on, &ctx.volume).await;
        return;
    }
    if ctx.bin.is_empty() {
        return;
    }
    let helper = |name: &str| format!("{}/{name}", ctx.bin);
    match effect {
        Effect::Sound(_) => {}
        Effect::Notify {
            urgency,
            title,
            body,
        } => spawn(
            ctx,
            &helper("agent-notify"),
            &[&ctx.pane, urgency.as_str(), title, body],
        ),
        Effect::NotifyClose if ctx.notify_open => {
            if let Some(mut child) = command(ctx, &helper("agent-notify"), &["--close", &ctx.pane])
            {
                let _ = timeout(CLOSE_TIMEOUT, child.wait()).await;
            }
        }
        // Nothing open (the bash notifier keeps its id in the pane).
        Effect::NotifyClose => {}
        Effect::Bgwatch { agent_pid } => spawn(
            ctx,
            &helper("agent-bgwatch"),
            &[&ctx.pane, &agent_pid.to_string()],
        ),
        Effect::Blink => spawn(ctx, &helper("agent-blink"), &[]),
        // Handled by the daemon before the ack.
        Effect::Options(_) | Effect::RemindArm { .. } | Effect::RemindCancel | Effect::Watch => {}
    }
}

fn command(ctx: &Ctx, program: &str, args: &[&str]) -> Option<tokio::process::Child> {
    Command::new(program)
        .args(args)
        .env("TMUX", &ctx.tmux_env)
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Out of the daemon's process group, like `setsid -f` in bash.
        .process_group(0)
        .spawn()
        .ok()
}

/// Starts a helper and lets it run; it is reaped when it ends.
fn spawn(ctx: &Ctx, program: &str, args: &[&str]) {
    if let Some(mut child) = command(ctx, program, args) {
        tokio::task::spawn_local(async move {
            let _ = child.wait().await;
        });
    }
}
