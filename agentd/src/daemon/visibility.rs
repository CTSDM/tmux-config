//! V1-V2: which tmux client has keyboard focus, and so whether the user sees
//! a pane. Hyprland is asked over its request socket, not through `hyprctl`.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::tmux::{Client, Read};
use crate::core::Visibility;
use crate::procfs;

/// Hyprland answers at once; a hung compositor must not hold events.
const HYPR_TIMEOUT: Duration = Duration::from_millis(500);
/// As `ag_descends_from`: the client's process and 7 ancestors.
const ANCESTORS: usize = 8;

/// The test seams and Hyprland's instance, read once when the daemon
/// starts (contract §14).
#[derive(Debug, Clone)]
pub struct Seams {
    /// `AG_FOCUS_CLIENT`: a client name, or `none`.
    pub focus_client: Option<String>,
    pub hypr_signature: Option<String>,
    pub runtime_dir: PathBuf,
}

impl Seams {
    pub fn from_env() -> Self {
        let nonempty = |name: &str| env::var(name).ok().filter(|v| !v.is_empty());
        let runtime_dir = nonempty("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()))
            });
        Seams {
            focus_client: nonempty("AG_FOCUS_CLIENT"),
            hypr_signature: nonempty("HYPRLAND_INSTANCE_SIGNATURE"),
            runtime_dir,
        }
    }
}

/// V1. Control-mode clients never count (spike-control-mode.md).
pub async fn focused_client<'a>(seams: &Seams, clients: &'a [Client]) -> Option<&'a Client> {
    let mut candidates = clients.iter().filter(|c| !c.control);
    if let Some(name) = &seams.focus_client {
        return (name != "none")
            .then(|| candidates.find(|c| c.name == *name))
            .flatten();
    }
    match hypr_active_pid(seams).await {
        Some(win) => {
            let win = u32::try_from(win).ok()?;
            candidates.find(|c| procfs::descends_from(c.pid, win, ANCESTORS))
        }
        None => candidates.find(|c| c.flags.split(',').any(|f| f == "focused")),
    }
}

/// V2.
pub fn visibility(focused: Option<&Client>, read: &Read) -> Visibility {
    match focused {
        Some(c) if c.session == read.pane.session => {
            if read.window_active && read.pane_active {
                Visibility::Visible
            } else {
                Visibility::Session
            }
        }
        _ => Visibility::Away,
    }
}

/// The pid of Hyprland's active window; `None` when Hyprland can't be asked
/// or has no active window.
async fn hypr_active_pid(seams: &Seams) -> Option<i64> {
    let reply = hypr(seams, "j/activewindow").await?;
    let json: serde_json::Value = serde_json::from_slice(&reply).ok()?;
    json.get("pid")?.as_i64()
}

/// One request to Hyprland's request socket, and its reply. As
/// `ag_hyprctl`: a stale or missing signature falls back to the most recent
/// instance.
pub async fn hypr(seams: &Seams, request: &str) -> Option<Vec<u8>> {
    let dir = seams.runtime_dir.join("hypr");
    let named = seams
        .hypr_signature
        .as_ref()
        .map(|sig| dir.join(sig).join(".socket.sock"))
        .filter(|p| p.exists());
    let socket = match named {
        Some(p) => p,
        None => newest_entry(&dir)?.join(".socket.sock"),
    };
    let ask = async {
        let mut stream = UnixStream::connect(&socket).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.ok()?;
        Some(reply)
    };
    timeout(HYPR_TIMEOUT, ask).await.ok().flatten()
}

/// `ls -t <dir> | head -n1`.
fn newest_entry(dir: &std::path::Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(name: &str, flags: &str, session: &str, control: bool) -> Client {
        Client {
            name: name.into(),
            pid: 0,
            flags: flags.into(),
            session: session.into(),
            control,
        }
    }

    fn seams(focus: Option<&str>) -> Seams {
        Seams {
            focus_client: focus.map(String::from),
            hypr_signature: None,
            runtime_dir: PathBuf::from("/nonexistent-agentd-test"),
        }
    }

    fn read(session: &str, window_active: bool, pane_active: bool) -> Read {
        let mut r = Read::default();
        r.pane.session = session.into();
        r.window_active = window_active;
        r.pane_active = pane_active;
        r
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v1_seam_names_the_client() {
        let clients = [
            client("ctl", "focused,control-mode", "api", true),
            client("/dev/pts/1", "attached", "api", false),
        ];
        let got = focused_client(&seams(Some("/dev/pts/1")), &clients).await;
        assert_eq!(got.map(|c| c.name.as_str()), Some("/dev/pts/1"));
        assert!(
            focused_client(&seams(Some("none")), &clients)
                .await
                .is_none()
        );
        assert!(
            focused_client(&seams(Some("nope")), &clients)
                .await
                .is_none()
        );
        // A control client never counts, even named.
        assert!(
            focused_client(&seams(Some("ctl")), &clients)
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v1_focused_flag_without_hyprland() {
        let clients = [
            client("ctl", "attached,focused,control-mode", "x", true),
            client("a", "attached,UTF-8", "api", false),
            client("b", "attached,focused,UTF-8", "api", false),
        ];
        let got = focused_client(&seams(None), &clients).await;
        assert_eq!(got.map(|c| c.name.as_str()), Some("b"));
    }

    #[test]
    fn v2_visible_session_away() {
        let c = client("a", "focused", "api", false);
        assert_eq!(
            visibility(Some(&c), &read("api", true, true)),
            Visibility::Visible
        );
        assert_eq!(
            visibility(Some(&c), &read("api", true, false)),
            Visibility::Session
        );
        assert_eq!(
            visibility(Some(&c), &read("api", false, true)),
            Visibility::Session
        );
        assert_eq!(
            visibility(Some(&c), &read("web", true, true)),
            Visibility::Away
        );
        assert_eq!(visibility(None, &read("api", true, true)), Visibility::Away);
    }
}
