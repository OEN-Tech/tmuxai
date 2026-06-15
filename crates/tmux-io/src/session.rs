use tokio::process::Command;
use std::process::Stdio;

pub struct SessionConfig {
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub width: u16,
    pub height: u16,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { name: String::new(), command: String::new(), cwd: ".".into(), width: 200, height: 50 }
    }
}

pub(crate) async fn tmux(args: &[&str]) -> Result<String, String> {
    let out = Command::new("tmux")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("tmux exec: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        // "no server running" and "session not found" are not fatal
        if err.contains("no server") || err.contains("not found") {
            Ok(String::new())
        } else {
            Err(format!("tmux {}: {err}", args.join(" ")))
        }
    }
}

pub async fn create_session(cfg: &SessionConfig) -> Result<(), String> {
    let w = cfg.width.to_string();
    let h = cfg.height.to_string();
    tmux(&[
        "new-session", "-d",
        "-s", &cfg.name,
        "-c", &cfg.cwd,
        "-x", &w,
        "-y", &h,
        &cfg.command,
    ]).await?;
    // Wait for process to start
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}

pub async fn kill_session(name: &str) -> Result<(), String> {
    tmux(&["kill-session", "-t", name]).await?;
    Ok(())
}

pub async fn session_exists(name: &str) -> Result<bool, String> {
    let out = tmux(&["has-session", "-t", name]).await;
    Ok(out.is_ok())
}

pub async fn list_sessions() -> Result<Vec<String>, String> {
    let out = tmux(&["list-sessions", "-F", "#{session_name}"]).await?;
    Ok(out.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect())
}
