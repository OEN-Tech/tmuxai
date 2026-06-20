use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn socket_path() -> String {
    std::env::var("TMUX_AI_SOCKET").unwrap_or_else(|_| "/tmp/tmux-ai-parser.sock".into())
}

pub fn profiles_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TMUX_AI_PROFILES") {
        return PathBuf::from(p);
    }
    let deployed = dirs_home().join(".config/tmuxai/profiles");
    if deployed.exists() { deployed } else { PathBuf::from("profiles") }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// One request/response round-trip on its own connection. Auto-starts the
/// daemon when the socket is dead.
pub async fn request(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let sock = socket_path();
    let stream = match UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(_) => {
            ensure_daemon().await?;
            UnixStream::connect(&sock).await.map_err(|e| format!("connect {sock}: {e}"))?
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(read_half);
    let mut resp = String::new();
    reader.read_line(&mut resp).await.map_err(|e| format!("read: {e}"))?;
    if resp.trim().is_empty() {
        return Err("daemon closed connection without responding".into());
    }
    let parsed: serde_json::Value = serde_json::from_str(&resp).map_err(|e| format!("parse: {e}"))?;
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        return Err(err.to_string());
    }
    Ok(parsed)
}

async fn ensure_daemon() -> Result<(), String> {
    let sock = socket_path();
    // Single-flight cold-start: the lock winner spawns the daemon; losers just
    // poll for the socket. Use an flock (auto-released on exit/crash) rather than
    // an O_EXCL existence file — a winner that crashed mid-spawn used to leave the
    // file behind, turning every later client into a non-winner that polls, fails
    // with "daemon did not come up", and only then clears the stale lock (a wave
    // of spurious failures). flock self-heals: a crashed winner's lock is released
    // by the kernel, so the very next client wins and retries the spawn.
    let lock_path = format!("{sock}.start-lock");
    let lock_file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).ok();
    let won = match &lock_file {
        Some(f) => {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        }
        None => false,
    };
    if won {
        // lock_file (and its flock) stays held until this fn returns; on an early
        // return it drops here, releasing the lock for the next client.
        spawn_daemon_process()?;
    }
    // Check first (the daemon may already be up), then poll.
    let mut up = false;
    for i in 0..20 {
        if UnixStream::connect(&sock).await.is_ok() {
            up = true;
            break;
        }
        if i + 1 < 20 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    // `lock_file` is dropped here → flock released. No file to unlink (the anchor
    // file is harmless to leave; the lock, not the file, is the authority).
    drop(lock_file);
    if up {
        Ok(())
    } else {
        Err(format!("daemon did not come up on {sock}"))
    }
}

fn spawn_daemon_process() -> Result<(), String> {
    let bin = std::env::var("TMUX_AI_DAEMON").map(PathBuf::from).unwrap_or_else(|_| {
        std::env::current_exe().ok()
            .and_then(|p| p.parent().map(|d| d.join("tmux-ai-daemon")))
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("tmux-ai-daemon"))
    });
    eprintln!("[tmuxai] starting daemon: {}", bin.display());
    tokio::process::Command::new(&bin)
        .env("TMUX_AI_PROFILES", profiles_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn daemon {}: {e}", bin.display()))
}

/// Last AssistantText in an events array (events serialize externally tagged).
pub fn last_assistant_text(events: &[serde_json::Value]) -> Option<String> {
    events.iter().rev().find_map(|e| {
        e.get("AssistantText")
            .and_then(|a| a.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    })
}

pub fn exit_code_for_state(state: &str) -> i32 {
    match state {
        "timeout" => 2,
        "dead" => 1,
        _ => 0,
    }
}
