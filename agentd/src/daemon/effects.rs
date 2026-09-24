//! Effects. Sounds and notifications are in-process; of the bash helpers of
//! `@agents_bin` only agent-blink is left, until phase 4 (design.md,
//! "Migration"). Reminders and background shells are the daemon's own
//! (mod.rs).

use std::process::Stdio;
use std::rc::Rc;

use tokio::process::Command;

use super::notify::Notifier;
use super::sound;
use crate::core::Effect;

/// What effects need from the whole daemon.
pub struct Env {
    pub sound: sound::Config,
    pub notifier: Rc<Notifier>,
}

/// What the effects of one event need besides themselves.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub pane: String,
    /// `@agents_bin`, read with the event.
    pub bin: String,
    /// `$TMUX` for the helpers.
    pub tmux_env: String,
    /// `@agent_sound` is not `off`, and `@agent_sound_volume`.
    pub sound_on: bool,
    pub volume: String,
}

/// Runs one effect; the pane's effects run one after the other (O3).
pub async fn run(env: &Env, ctx: &Ctx, effect: &Effect) {
    match effect {
        Effect::Sound(name) => sound::play(&env.sound, name, ctx.sound_on, &ctx.volume).await,
        Effect::Notify {
            urgency,
            title,
            body,
        } => {
            env.notifier
                .show(&ctx.pane, *urgency, title, body, &ctx.bin, &ctx.tmux_env)
                .await
        }
        Effect::NotifyClose => env.notifier.close(&ctx.pane).await,
        Effect::Blink if !ctx.bin.is_empty() => {
            spawn(ctx, &format!("{}/agent-blink", ctx.bin), &[])
        }
        // Handled by the daemon before the ack, or no helpers to run.
        _ => {}
    }
}

/// Starts a helper out of the daemon's process group (like `setsid -f` in
/// bash) and lets it run; it is reaped when it ends.
fn spawn(ctx: &Ctx, program: &str, args: &[&str]) {
    let child = Command::new(program)
        .args(args)
        .env("TMUX", &ctx.tmux_env)
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
