mod server;
mod session_mgr;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() {
    let socket_path = std::env::var("TMUX_AI_SOCKET")
        .unwrap_or_else(|_| "/tmp/tmux-ai-parser.sock".into());
    let profiles_dir = std::env::var("TMUX_AI_PROFILES")
        .unwrap_or_else(|_| "profiles".into());
    let db_path = std::env::var("TMUX_AI_DB")
        .ok()
        .map(PathBuf::from);

    eprintln!("[daemon] profiles: {profiles_dir}");
    eprintln!("[daemon] socket: {socket_path}");
    if let Some(ref db) = db_path {
        eprintln!("[daemon] db: {}", db.display());
    }

    let mgr = match session_mgr::SessionManager::new(
        Path::new(&profiles_dir),
        db_path.as_deref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[daemon] fatal: {e}");
            std::process::exit(1);
        }
    };

    let mgr = Arc::new(Mutex::new(mgr));

    // Handle SIGINT/SIGTERM — kill all sessions
    let mgr_shutdown = mgr.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\n[daemon] shutting down...");
        mgr_shutdown.lock().await.kill_all().await;
        std::process::exit(0);
    });

    // Reap idle sessions so orphans (e.g. fan-out workers left behind) don't
    // accumulate. TMUX_AI_IDLE_REAP_SECS=0 disables it; default 30 min.
    let reap_secs: u64 = std::env::var("TMUX_AI_IDLE_REAP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800);
    if reap_secs > 0 {
        let max_idle = std::time::Duration::from_secs(reap_secs);
        let mgr_reaper = mgr.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                // Remove stale entries under a SHORT manager lock, then kill
                // their tmux sessions with the lock released so a batch of
                // orphans can't stall other requests behind the kill loop.
                let stale = { mgr_reaper.lock().await.take_idle(max_idle) };
                for (name, handle) in stale {
                    let _ = handle.lock().await.tmux.kill().await;
                    eprintln!("[daemon] reaped idle session: {name}");
                }
            }
        });
        eprintln!("[daemon] idle reaper: sessions idle > {reap_secs}s are killed");
    }

    if let Err(e) = server::run_server(Path::new(&socket_path), mgr).await {
        eprintln!("[daemon] fatal: {e}");
        std::process::exit(1);
    }
}
