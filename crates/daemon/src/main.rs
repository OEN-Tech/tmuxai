mod server;
mod session_mgr;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Acquire an exclusive, non-blocking advisory lock on `path`. Returns the open
/// File (whose fd holds the lock for as long as it stays open) on success. On
/// failure: `Err(true)` if another live process already holds it, `Err(false)`
/// if the lock file itself couldn't be opened.
fn acquire_single_instance_lock(path: &str) -> Result<std::fs::File, bool> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .map_err(|_| false)?;
    // LOCK_NB → fail immediately (EWOULDBLOCK) instead of blocking if another
    // daemon holds it. flock is released automatically when the fd is closed
    // (process exit/crash), so there is no stale-lock to clean up.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(file)
    } else {
        Err(true)
    }
}

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

    // Single-instance lock. The connect-probe in run_server only catches a LATE
    // starter (after the winner is already listening); two daemons cold-starting
    // SIMULTANEOUSLY both probe before either binds, then both remove_file+bind,
    // clobbering each other's socket (the observed "2 daemons" flap). An
    // exclusive flock is atomic, so exactly one wins the race. The lock is tied
    // to the fd and auto-released on exit/crash — no stale-pidfile problem. Held
    // for the whole process via `_daemon_lock` (lives until main returns, i.e.
    // never, since run_server loops).
    let lock_path = format!("{socket_path}.lock");
    let _daemon_lock = match acquire_single_instance_lock(&lock_path) {
        Ok(f) => f,
        Err(held) => {
            if held {
                eprintln!("[daemon] another daemon already holds {lock_path}; exiting");
                std::process::exit(0);
            } else {
                eprintln!("[daemon] fatal: cannot open lock {lock_path}");
                std::process::exit(1);
            }
        }
    };

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

#[cfg(test)]
mod lock_tests {
    use super::acquire_single_instance_lock;

    #[test]
    fn second_acquire_is_denied_while_first_is_held() {
        let path = std::env::temp_dir().join(format!("tmuxai-lock-{}.lock", std::process::id()));
        let p = path.to_str().unwrap();
        let _first = acquire_single_instance_lock(p).expect("first acquire succeeds");
        // A second open+flock of the same path is denied while the first fd holds it
        // (POSIX: flock locks are per open-file-description, denied even same-process).
        assert!(matches!(acquire_single_instance_lock(p), Err(true)), "second must be held");
        drop(_first);
        // After the holder drops, the lock is free again.
        let _again = acquire_single_instance_lock(p).expect("re-acquire after release");
        std::fs::remove_file(p).ok();
    }
}
