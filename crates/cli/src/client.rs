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
    // Single-flight: the O_EXCL lock winner spawns the daemon; losers just
    // poll for the socket. Prevents N concurrent cold-starts (fanout pool
    // spawn) from launching N daemons that clobber each other's socket file.
    let lock_path = std::path::PathBuf::from(format!("{sock}.start-lock"));
    let won = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .is_ok();
    if won {
        if let Err(e) = spawn_daemon_process() {
            let _ = std::fs::remove_file(&lock_path);
            return Err(e);
        }
    }
    let mut up = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if UnixStream::connect(&sock).await.is_ok() {
            up = true;
            break;
        }
    }
    if won {
        let _ = std::fs::remove_file(&lock_path);
    }
    if up {
        Ok(())
    } else {
        if !won {
            // The lock holder may have crashed and left a stale lock; clear it
            // so the next invocation can attempt the spawn again.
            let _ = std::fs::remove_file(&lock_path);
        }
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
