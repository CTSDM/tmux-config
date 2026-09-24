//! Which tmux server a daemon belongs to, and where its runtime files live
//! (design.md, "Daemon identity").

use std::ffi::OsStr;
use std::fmt::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The tmux server's socket path from `$TMUX` (`<socket>,<pid>,<session>`),
/// like `${TMUX%%,*}` in bash. `None` when there is none.
pub fn server_socket(tmux: &OsStr) -> Option<&Path> {
    let bytes = tmux.as_bytes();
    let end = bytes.iter().position(|&b| b == b',').unwrap_or(bytes.len());
    (end > 0).then(|| Path::new(OsStr::from_bytes(&bytes[..end])))
}

/// `<socket basename>-<first 8 hex digits of the SHA-256 of its full path>`,
/// e.g. `default-7cbd633a`: readable, and still unique when two servers use
/// sockets with the same name in different directories.
pub fn server_id(socket: &Path) -> String {
    let name = socket.file_name().unwrap_or_default().to_string_lossy();
    let digest = Sha256::digest(socket.as_os_str().as_bytes());
    let mut id = format!("{name}-");
    for byte in &digest[..4] {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// `$XDG_RUNTIME_DIR/tmux-agents`, the folder the bash scripts use too;
/// `/tmp/tmux-agents` when the variable is unset or empty (as agent-lib.sh).
pub fn runtime_dir(xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    let base = xdg_runtime_dir
        .filter(|d| !d.is_empty())
        .unwrap_or(OsStr::new("/tmp"));
    Path::new(base).join("tmux-agents")
}

/// The daemon's runtime files for one tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn new(runtime_dir: &Path, server_id: &str) -> Self {
        Paths {
            socket: runtime_dir.join(format!("agentd-{server_id}.sock")),
            lock: runtime_dir.join(format!("agentd-{server_id}.lock")),
            state: runtime_dir.join(format!("agentd-{server_id}.state.json")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_from_tmux_variable() {
        let socket = |s: &str| server_socket(OsStr::new(s)).map(Path::to_path_buf);
        assert_eq!(
            socket("/tmp/tmux-1000/default,4242,0"),
            Some("/tmp/tmux-1000/default".into())
        );
        assert_eq!(
            socket("/tmp/tmux-1000/default"),
            Some("/tmp/tmux-1000/default".into())
        );
        assert_eq!(socket(""), None);
        assert_eq!(socket(",4242,0"), None);
    }

    #[test]
    fn id_is_basename_and_hash_of_full_path() {
        // printf %s /tmp/tmux-1000/default | sha256sum
        assert_eq!(
            server_id(Path::new("/tmp/tmux-1000/default")),
            "default-7cbd633a"
        );
        assert_eq!(
            server_id(Path::new("/run/user/1000/tmux/work")),
            "work-29a052c7"
        );
    }

    #[test]
    fn runtime_dir_defaults_like_bash() {
        assert_eq!(
            runtime_dir(Some(OsStr::new("/run/user/1000"))),
            PathBuf::from("/run/user/1000/tmux-agents")
        );
        assert_eq!(
            runtime_dir(Some(OsStr::new(""))),
            PathBuf::from("/tmp/tmux-agents")
        );
        assert_eq!(runtime_dir(None), PathBuf::from("/tmp/tmux-agents"));
    }

    #[test]
    fn paths_per_server() {
        let paths = Paths::new(Path::new("/run/user/1000/tmux-agents"), "default-7cbd633a");
        assert_eq!(
            paths.socket,
            PathBuf::from("/run/user/1000/tmux-agents/agentd-default-7cbd633a.sock")
        );
        assert_eq!(
            paths.lock,
            PathBuf::from("/run/user/1000/tmux-agents/agentd-default-7cbd633a.lock")
        );
        assert_eq!(
            paths.state,
            PathBuf::from("/run/user/1000/tmux-agents/agentd-default-7cbd633a.state.json")
        );
    }
}
