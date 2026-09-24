//! S9, playing a sound, as agents/bin/agent-sound does: the file, the
//! player, and one debounce per user shared with bash and other daemons
//! through `$XDG_RUNTIME_DIR/tmux-agents/sound.last` under the same flock.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use tokio::process::Command;
use tokio::time::sleep;

/// Within this of the last sound, only a more important one plays.
const DEBOUNCE_US: u64 = 2_500_000;
/// As `flock -w 2`.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const EXTENSIONS: [&str; 5] = ["wav", "ogg", "oga", "mp3", "flac"];

/// Where sounds come from and go, read once when the daemon starts.
#[derive(Debug, Clone)]
pub struct Config {
    /// `AG_SOUNDS`, else `$XDG_DATA_HOME/tmux-agents/sounds`.
    pub dir: PathBuf,
    /// `AG_SOUND_PLAYER`.
    pub player: Option<String>,
    /// `AG_SINK` (tests): the line instead of the player.
    pub sink: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR/tmux-agents`, where bash keeps sound.last too.
    pub run: PathBuf,
}

impl Config {
    pub fn from_env(run: PathBuf) -> Config {
        let nonempty = |n: &str| env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let data = nonempty("XDG_DATA_HOME")
            .or_else(|| nonempty("HOME").map(|h| h.join(".local/share")))
            .unwrap_or_default();
        Config {
            dir: nonempty("AG_SOUNDS").unwrap_or_else(|| data.join("tmux-agents/sounds")),
            player: env::var("AG_SOUND_PLAYER").ok().filter(|p| !p.is_empty()),
            sink: nonempty("AG_SINK"),
            run,
        }
    }
}

/// S9: the more important sounds win the debounce.
pub fn priority(name: &str) -> u8 {
    match name {
        "need-backup" | "report-in" | "wait-for-my-go" | "oh-man" | "come-to-papa" => 4,
        "ct-win" => 3,
        "enemy-down" | "lets-do-this" => 2,
        _ => 1,
    }
}

/// The first readable `<name>.<ext>` in the folder.
pub fn file(dir: &Path, name: &str) -> Option<PathBuf> {
    EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|p| File::open(p).is_ok())
}

/// `awk 'BEGIN { printf "%d", v * 100 }'`: a percentage, cut toward zero.
fn percent(volume: &str) -> i64 {
    (volume.trim().parse::<f64>().unwrap_or(0.0) * 100.0) as i64
}

/// The player's command line, as agent-sound chooses it.
pub fn player(
    file: &Path,
    volume: &str,
    custom: Option<&str>,
    installed: impl Fn(&str) -> bool,
) -> Vec<String> {
    let file = file.to_string_lossy().into_owned();
    if let Some(custom) = custom {
        return vec![custom.into(), file];
    }
    let v = percent(volume);
    if installed("pw-play") {
        vec!["pw-play".into(), "--volume".into(), volume.into(), file]
    } else if installed("ffplay") {
        [
            "ffplay",
            "-nodisp",
            "-autoexit",
            "-loglevel",
            "quiet",
            "-volume",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([v.to_string(), file])
        .collect()
    } else if installed("mpv") {
        vec![
            "mpv".into(),
            "--no-video".into(),
            "--really-quiet".into(),
            format!("--volume={v}"),
            file,
        ]
    } else {
        vec!["aplay".into(), "-q".into(), file]
    }
}

/// What the debounce says for a sound of `priority` at `now_us`, given the
/// line of sound.last (`<µs> <priority> [pid]`).
#[derive(Debug, PartialEq, Eq)]
pub enum Debounce {
    Skip,
    /// Play, stopping the previous player if it was cut short.
    Play {
        stop: Option<u32>,
    },
}

pub fn debounce(now_us: u64, last: &str, priority: u8) -> Debounce {
    let mut fields = last.split_whitespace();
    let (Some(at), prio, pid) = (
        fields.next().and_then(|t| t.parse::<u64>().ok()),
        fields
            .next()
            .and_then(|p| p.parse::<u8>().ok())
            .unwrap_or(0),
        fields
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .filter(|p| *p > 1),
    ) else {
        return Debounce::Play { stop: None };
    };
    if now_us.saturating_sub(at) >= DEBOUNCE_US {
        return Debounce::Play { stop: None };
    }
    if priority > prio {
        Debounce::Play { stop: pid }
    } else {
        Debounce::Skip
    }
}

fn installed(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|d| d.join(program).is_file()))
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Plays `name` unless switched off, missing or debounced. `enabled` is
/// `@agent_sound` not being `off`, `volume` is `@agent_sound_volume`.
pub async fn play(cfg: &Config, name: &str, enabled: bool, volume: &str) {
    if !enabled {
        return;
    }
    let Some(file) = file(&cfg.dir, name) else {
        return;
    };
    let volume = if volume.is_empty() { "0.8" } else { volume };
    let argv = player(&file, volume, cfg.player.as_deref(), installed);
    let priority = priority(name);

    let _ = fs::create_dir_all(&cfg.run);
    let Ok(lock) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(cfg.run.join("sound.lock"))
    else {
        return;
    };
    // flock -w 2, without blocking the daemon's thread.
    let deadline = Instant::now() + LOCK_WAIT;
    while flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err() {
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let last_path = cfg.run.join("sound.last");
    let now = now_us();
    let last = fs::read_to_string(&last_path).unwrap_or_default();
    let stop = match debounce(now, last.lines().next().unwrap_or(""), priority) {
        Debounce::Skip => return,
        Debounce::Play { stop } => stop,
    };
    if let Some(pid) = stop {
        stop_player(pid, &cfg.dir);
    }
    if let Some(sink) = &cfg.sink {
        let line = serde_json::json!({"t": now / 1000, "effect": "sound", "name": name});
        append_line(sink, &line.to_string());
        let _ = fs::write(&last_path, format!("{now} {priority}\n"));
        return;
    }
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match child {
        Ok(mut child) => {
            let pid = child.id().unwrap_or(0);
            let _ = fs::write(&last_path, format!("{now} {priority} {pid}\n"));
            tokio::task::spawn_local(async move {
                let _ = child.wait().await;
            });
        }
        Err(_) => {
            let _ = fs::write(&last_path, format!("{now} {priority}\n"));
        }
    }
}

/// A higher priority sound cuts the previous one off. Bash kills whatever
/// pid sound.last names; this checks it is still a player of our sounds.
fn stop_player(pid: u32, dir: &Path) {
    let playing = crate::procfs::entries(pid, "cmdline")
        .is_some_and(|argv| argv.iter().any(|a| a.starts_with(&*dir.to_string_lossy())));
    if let (true, Some(pid)) = (playing, rustix::process::Pid::from_raw(pid as i32)) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

/// One JSON line appended in one write (tests read the sink concurrently).
pub fn append_line(path: &Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s9_priorities() {
        for n in [
            "need-backup",
            "report-in",
            "wait-for-my-go",
            "oh-man",
            "come-to-papa",
        ] {
            assert_eq!(priority(n), 4, "{n}");
        }
        assert_eq!(priority("ct-win"), 3);
        assert_eq!((priority("enemy-down"), priority("lets-do-this")), (2, 2));
        assert_eq!(priority("fight-like-a-man"), 1);
    }

    #[test]
    fn s9_debounce() {
        let t = 10_000_000;
        assert_eq!(debounce(t, "", 1), Debounce::Play { stop: None });
        assert_eq!(debounce(t, "garbage", 1), Debounce::Play { stop: None });
        // Within 2.5 s: only a higher priority, which stops the previous player.
        assert_eq!(
            debounce(t, &format!("{} 2 4242", t - 1_000_000), 2),
            Debounce::Skip
        );
        assert_eq!(
            debounce(t, &format!("{} 2 4242", t - 1_000_000), 1),
            Debounce::Skip
        );
        assert_eq!(
            debounce(t, &format!("{} 2 4242", t - 1_000_000), 4),
            Debounce::Play { stop: Some(4242) }
        );
        // Written by a sink run: no pid.
        assert_eq!(
            debounce(t, &format!("{} 2", t - 1_000_000), 4),
            Debounce::Play { stop: None }
        );
        // After 2.5 s, anything.
        assert_eq!(
            debounce(t, &format!("{} 4 4242", t - 2_500_000), 1),
            Debounce::Play { stop: None }
        );
    }

    #[test]
    fn s9_player_choice() {
        let f = Path::new("/s/need-backup.wav");
        let only = |name: &'static str| move |p: &str| p == name;
        assert_eq!(
            player(f, "0.8", Some("/t/player"), only("pw-play")),
            ["/t/player", "/s/need-backup.wav"]
        );
        assert_eq!(
            player(f, "0.8", None, only("pw-play")),
            ["pw-play", "--volume", "0.8", "/s/need-backup.wav"]
        );
        assert_eq!(
            player(f, "0.55", None, only("ffplay")),
            [
                "ffplay",
                "-nodisp",
                "-autoexit",
                "-loglevel",
                "quiet",
                "-volume",
                "55",
                "/s/need-backup.wav"
            ]
        );
        assert_eq!(
            player(f, "1", None, only("mpv")),
            [
                "mpv",
                "--no-video",
                "--really-quiet",
                "--volume=100",
                "/s/need-backup.wav"
            ]
        );
        assert_eq!(
            player(f, "0.8", None, |_| false),
            ["aplay", "-q", "/s/need-backup.wav"]
        );
        assert_eq!(percent("x"), 0);
    }

    #[test]
    fn s9_file_lookup() {
        let dir = std::env::temp_dir().join(format!("agentd-sounds-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(file(&dir, "oh-man").is_none());
        fs::write(dir.join("oh-man.mp3"), "").unwrap();
        fs::write(dir.join("oh-man.ogg"), "").unwrap();
        assert_eq!(file(&dir, "oh-man").unwrap(), dir.join("oh-man.ogg"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s9_sink_and_shared_debounce() {
        let dir = std::env::temp_dir().join(format!("agentd-play-{}", std::process::id()));
        let sounds = dir.join("sounds");
        fs::create_dir_all(&sounds).unwrap();
        for n in ["enemy-down", "need-backup", "fight-like-a-man"] {
            fs::write(sounds.join(format!("{n}.wav")), "").unwrap();
        }
        let cfg = Config {
            dir: sounds,
            player: Some("/bin/false".into()),
            sink: Some(dir.join("sink.jsonl")),
            run: dir.join("run"),
        };
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                play(&cfg, "enemy-down", false, "").await; // @agent_sound off
                play(&cfg, "enemy-down", true, "").await;
                play(&cfg, "fight-like-a-man", true, "").await; // lower: debounced
                play(&cfg, "need-backup", true, "").await; // higher: plays
                play(&cfg, "no-such-sound", true, "").await;
            })
            .await;
        let sink = fs::read_to_string(dir.join("sink.jsonl")).unwrap();
        let names: Vec<String> = sink
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names, ["enemy-down", "need-backup"]);
        assert!(
            sink.lines().next().unwrap().starts_with("{\"t\":"),
            "{sink}"
        );
        let last = fs::read_to_string(dir.join("run/sound.last")).unwrap();
        assert!(last.trim().ends_with(" 4"), "{last}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
