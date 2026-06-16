// RC-1 regression: `kill_session` must reap a SIGHUP-immune pane descendant —
// the exact failure mode that leaked ~74 idle `gemini` node TUIs (gemini ignores
// the pane's SIGHUP and re-execs its heavy child into its own process group).
// Skips cleanly if tmux is unavailable (e.g. a minimal CI image).
use std::time::Duration;
use tokio::process::Command;

async fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn alive(pid: i32) -> bool {
    // `kill -0` exits 0 iff a process with that pid exists.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn kill_session_reaps_sighup_immune_descendant() {
    if !tmux_available().await {
        eprintln!("skip: tmux unavailable");
        return;
    }
    let name = format!("tmuxai-reap-it-{}", std::process::id());
    let dir = std::env::temp_dir().join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("survivor.pid");

    // The pane backgrounds a HUP-ignoring sleeper (mimics gemini's TUI),
    // records its pid, then idles so the pane stays open.
    let cmd = format!(
        "sh -c 'trap \"\" HUP; sleep 300' & echo $! > {pf}; sleep 600",
        pf = pidfile.display()
    );
    tmux_ai_io::session::create_session(&tmux_ai_io::session::SessionConfig {
        name: name.clone(),
        command: cmd,
        cwd: ".".into(),
        ..Default::default()
    })
    .await
    .expect("create_session");

    // Wait for the survivor pid to be recorded.
    let mut survivor: Option<i32> = None;
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&pidfile) {
            if let Ok(p) = s.trim().parse::<i32>() {
                survivor = Some(p);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let survivor = survivor.expect("survivor pid file never written");
    assert!(alive(survivor).await, "survivor should be alive before kill");

    // The fix under test.
    tmux_ai_io::session::kill_session(&name).await.expect("kill_session");

    // G1 regression: the session must now read as GONE. A too-broad tmux()
    // "can't find" tolerance would make has-session return Ok("") and this
    // flip to true, leaving the daemon unable to detect dead sessions.
    assert!(
        !tmux_ai_io::session::session_exists(&name).await.unwrap(),
        "session must not exist after kill_session"
    );

    // The SIGHUP-immune survivor must now be dead (SIGKILL'd by the tree reap).
    let mut dead = false;
    for _ in 0..40 {
        if !alive(survivor).await {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Best-effort cleanup regardless of outcome.
    let _ = Command::new("kill").args(["-9", &survivor.to_string()]).output().await;
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        dead,
        "survivor {survivor} ignored SIGHUP and was NOT reaped by kill_session"
    );
}
