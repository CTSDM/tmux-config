//! The debug log, as the bash scripts keep it: lines go to
//! `errors.log` in the state folder only while `debug` exists there.

use std::fs::OpenOptions;
use std::io::Write;

use crate::identity;

pub fn log(message: &str) {
    let Some(dir) = identity::state_dir() else {
        return;
    };
    if !dir.join("debug").exists() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("errors.log"))
    {
        let _ = writeln!(f, "agentd[{}] {message}", std::process::id());
    }
}
