//! Remote panes (design.md, "Remote panes"; issue #4, option B): a local
//! pane whose shell runs on another host, kept alive there by `agentd hold`
//! as dtach would, with its agents' events sent to the local daemon as the
//! pane's own.
//!
//! - `hold`: `agentd hold [<name>]` and the holder, on the server.
//! - `frame`: what goes between them.
//! - the local end, `agentd remote`, is the desktop's (agentd's remote/attach.rs).

pub mod frame;
pub mod hold;

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// The environment variable of a held program: its holder's socket. A hook
/// that sees it sends its event there (hook.rs).
pub const HOLD_VAR: &str = "AGENTD_HOLD";

/// A name is a file name part and a word in a shell command.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Where holders keep their sockets: `${TMUX_TMPDIR:-/tmp}/agentd-<uid>`,
/// as tmux does, since logind removes `$XDG_RUNTIME_DIR` at logout and a
/// held shell outlives the ssh. `AGENTD_HOLD_DIR` moves it (tests).
pub fn dir() -> PathBuf {
    let var = |name| env::var_os(name).filter(|v| !v.is_empty());
    if let Some(d) = var("AGENTD_HOLD_DIR") {
        return PathBuf::from(d);
    }
    let base = var("TMUX_TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.join(format!("agentd-{}", rustix::process::getuid().as_raw()))
}

/// Creates the folder, private, and refuses one that isn't ours and private.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // Not followed: a link to a private folder of ours would pass, and could
    // be swapped for someone else's between the check and the use.
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir()
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other(format!(
            "{} is not private to this user",
            dir.display()
        )));
    }
    Ok(())
}

/// Connects to a holder's socket, only if a process of this user answers:
/// whatever the folder looks like, a socket someone else put there gets no
/// keys and no events.
pub fn connect(socket: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    same_user(&stream)?;
    Ok(stream)
}

/// The other end of `stream` runs as this user.
pub fn same_user(stream: &UnixStream) -> io::Result<()> {
    let peer = rustix::net::sockopt::socket_peercred(stream)?;
    if peer.uid == rustix::process::getuid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a socket of another user",
        ))
    }
}

/// The pid of the process at the other end of `stream`.
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let peer = rustix::net::sockopt::socket_peercred(stream).ok()?;
    u32::try_from(peer.pid.as_raw_nonzero().get()).ok()
}

/// A Unix socket path must fit in `sun_path` (108 bytes with its NUL).
pub const MAX_SOCKET_PATH: usize = 107;

pub fn socket(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("hold-{name}.sock"))
}

pub fn lock(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("hold-{name}.lock"))
}

/// The held program's last output, and where it starts in all it wrote.
pub struct Ring {
    data: VecDeque<u8>,
    start: u64,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring {
            data: VecDeque::new(),
            start: 0,
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        let over = self.data.len().saturating_sub(self.cap);
        self.data.drain(..over);
        self.start += over as u64;
    }

    /// Where the output written so far ends.
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// What a client that has shown everything up to `have` is missing, and
    /// where that starts. A new pane (`None`), or one that fell too far
    /// behind, gets all that is kept, from its first full line: bytes cut in
    /// the middle of an escape sequence would show as garbage.
    pub fn since(&self, have: Option<u64>) -> (u64, Vec<u8>) {
        let from = match have {
            Some(h) if (self.start..=self.end()).contains(&h) => (h - self.start) as usize,
            _ if self.start == 0 => 0,
            _ => self
                .data
                .iter()
                .position(|&b| b == b'\n')
                .map_or(0, |i| i + 1),
        };
        let bytes: Vec<u8> = self.data.range(from..).copied().collect();
        (self.start + from as u64, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_is_ours_private_and_no_link() {
        let base = std::env::temp_dir().join(format!("agentd-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real");
        ensure_dir(&real).unwrap();
        ensure_dir(&real).unwrap();
        // A link to it passes every other check, and is refused.
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        assert!(ensure_dir(&base.join("link")).is_err());
        // Not private.
        let open = base.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(ensure_dir(&open).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn names() {
        assert!(valid_name("api"));
        assert!(valid_name("api-2.work_x"));
        assert!(!valid_name(""));
        assert!(!valid_name(".x"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a;b"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn ring_keeps_the_last_bytes_and_their_place() {
        let mut r = Ring::new(8);
        r.push(b"abc");
        assert_eq!(r.since(None), (0, b"abc".to_vec()));
        assert_eq!(r.since(Some(1)), (1, b"bc".to_vec()));
        assert_eq!(r.since(Some(3)), (3, Vec::new()));
        r.push(b"de\nfghij");
        assert_eq!(r.end(), 11);
        // Kept: "de\nfghij" from 3.
        assert_eq!(r.since(Some(9)), (9, b"ij".to_vec()));
        // Too far behind, new, or from another holder: from the first full line.
        assert_eq!(r.since(Some(1)), (6, b"fghij".to_vec()));
        assert_eq!(r.since(None), (6, b"fghij".to_vec()));
        assert_eq!(r.since(Some(99)), (6, b"fghij".to_vec()));
        // No full line: all of it.
        let mut r = Ring::new(4);
        r.push(b"abcdef");
        assert_eq!(r.since(None), (2, b"cdef".to_vec()));
    }
}
