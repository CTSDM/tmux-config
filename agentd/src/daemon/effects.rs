//! Effects: sounds and notifications, in-process. Reminders, background
//! shells and the blink are the daemon's own (mod.rs, blink.rs).

use std::rc::Rc;

use serde_json::json;

use super::bridge::Link;
use super::notify::Notifier;
use super::sound;
use crate::core::Effect;

/// What effects need from the whole daemon.
pub struct Env {
    pub sound: sound::Config,
    pub notifier: Rc<Notifier>,
    /// The desktop bridge: sounds play there when it is up.
    pub link: Option<Rc<Link>>,
}

/// What the effects of one event need besides themselves.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub pane: String,
    /// `@agents_bin`, read with the event.
    pub bin: String,
    /// `$TMUX` for agent-jump, which a notification's click runs.
    pub tmux_env: String,
    /// `@agent_sound` is not `off`, and `@agent_sound_volume`.
    pub sound_on: bool,
    pub volume: String,
}

/// Runs one effect; the pane's effects run one after the other (O3).
pub async fn run(env: &Env, ctx: &Ctx, effect: &Effect) {
    match effect {
        Effect::Sound(name) => {
            if ctx.sound_on
                && env.sound.sink.is_none()
                && let Some(link) = &env.link
                && link
                    .send(&json!({"sound": {"name": name, "volume": ctx.volume}}))
                    .await
            {
                return;
            }
            sound::play(&env.sound, name, ctx.sound_on, &ctx.volume).await
        }
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
        // Handled by the daemon before the ack.
        _ => {}
    }
}
