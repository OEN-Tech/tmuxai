//! `tmuxai watch` — a live fleet dashboard, and `tmuxai logs` — raw pane stream.
//!
//! `watch` is a read-only monitor: it goes through the daemon's existing
//! `list_sessions` + per-session `poll`, so it's safe to run alongside an
//! orchestration (per-session locks serialize it). Run it in a SEPARATE
//! terminal/tmux pane — the live loop redraws until Ctrl-C.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

use crate::client;

const LAST_COL: usize = 64;
/// Per-session poll cap. The daemon holds a session's lock for the whole
/// duration of an in-flight `wait`/`send`, so a poll on a session another
/// client is actively driving would block. A monitor must never hang on a busy
/// session — cap the poll and render it "busy" instead.
const POLL_TIMEOUT: Duration = Duration::from_millis(1500);

/// One dashboard frame as a string (roster from list_sessions, state per
/// session from a concurrent poll).
async fn render_frame() -> String {
    let roster = match client::request(json!({"list_sessions": true})).await {
        Ok(v) => v.get("sessions").and_then(|s| s.as_array()).cloned().unwrap_or_default(),
        Err(e) => return format!("  (daemon unreachable: {e})\n"),
    };
    if roster.is_empty() {
        return "  no sessions — spawn one with `tmuxai spawn <name> --profile <p>`\n".to_string();
    }

    // Poll every session concurrently.
    let mut set = tokio::task::JoinSet::new();
    for s in &roster {
        let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
        let profile = s.get("profile").and_then(|p| p.as_str()).unwrap_or("?").to_string();
        set.spawn(async move {
            // Cap the poll: if the session lock is held by an active wait/send,
            // don't block the frame — report it busy.
            match tokio::time::timeout(POLL_TIMEOUT, client::request(json!({"poll": name.clone()}))).await {
                Ok(Ok(v)) => {
                    let st = v.get("state").and_then(|s| s.as_str()).unwrap_or("?").to_string();
                    let events = v.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default();
                    let line = client::last_assistant_text(&events).unwrap_or_default();
                    (name, profile, st, line)
                }
                Ok(Err(e)) => (name, profile, "error".to_string(), e),
                Err(_) => (name, profile, "busy".to_string(), "(active — held by send/wait)".to_string()),
            }
        });
    }

    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        let row = joined.unwrap_or_else(|_| ("?".into(), "?".into(), "error".into(), "join".into()));
        rows.push(row);
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    out.push_str(&format!("  {:<16} {:<12} {:<10} {}\n", "SESSION", "PROFILE", "STATE", "LAST LINE"));
    out.push_str(&format!("  {}\n", "─".repeat(16 + 12 + 10 + LAST_COL + 3)));
    for (name, profile, state, last) in rows {
        let glyph = match state.as_str() {
            "busy" => "●",
            "ready" => "✓",
            "question" => "?",
            "timeout" => "⌛",
            "dead" => "✗",
            _ => "·",
        };
        out.push_str(&format!(
            "  {:<16} {:<12} {} {:<8} {}\n",
            truncate(&name, 16), truncate(&profile, 12), glyph, truncate(&state, 8), one_line(&last, LAST_COL),
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Collapse to a single trimmed line, truncated.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max)
}

pub async fn run(once: bool, interval: u64) -> Result<(), String> {
    if once {
        print!("{}", render_frame().await);
        return Ok(());
    }
    let interval = Duration::from_secs(interval.max(1));
    loop {
        let frame = render_frame().await;
        // Clear screen + home cursor, then draw.
        print!("\x1b[2J\x1b[H  tmuxai fleet · refresh {}s · Ctrl-C to exit\n\n{}", interval.as_secs(), frame);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        tokio::time::sleep(interval).await;
    }
}

/// Path the daemon's ByteWatcher pipes a session's pane output to.
fn pipe_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tmux-ai-parser-{name}.pipe"))
}

pub async fn logs(name: &str, follow: bool, lines: usize) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let path = pipe_path(name);
    let initial = tokio::fs::read(&path).await.map_err(|e| {
        format!("no pane log for '{name}' at {} ({e}); the CURRENT daemon must have spawned it", path.display())
    })?;
    // Seed with the last `lines` lines.
    let text = String::from_utf8_lossy(&initial);
    let seeded: Vec<&str> = text.lines().collect();
    let start = seeded.len().saturating_sub(lines);
    let mut stdout = tokio::io::stdout();
    for l in &seeded[start..] {
        let _ = stdout.write_all(l.as_bytes()).await;
        let _ = stdout.write_all(b"\n").await;
    }
    let _ = stdout.flush().await;
    if !follow {
        return Ok(());
    }
    // Follow: print bytes appended past the current size.
    let mut offset = initial.len() as u64;
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => break, // session ended / log removed
        };
        let size = meta.len();
        if size > offset {
            if let Ok(buf) = read_from(&path, offset).await {
                let _ = stdout.write_all(&buf).await;
                let _ = stdout.flush().await;
                offset = size;
            }
        } else if size < offset {
            offset = size; // truncated/rotated
        }
    }
    Ok(())
}

async fn read_from(path: &PathBuf, offset: u64) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}
