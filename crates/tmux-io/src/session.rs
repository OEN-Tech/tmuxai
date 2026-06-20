use tokio::process::Command;
use std::process::Stdio;
use std::collections::{HashMap, HashSet};

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
        // "no server running" and "session not found" are not fatal.
        // NOTE: do NOT broaden this to tmux's "can't find <target>" wording —
        // has-session reports a missing session that way, and session_exists()
        // relies on that becoming an Err so it returns false. kill_session()
        // tolerates "can't find" LOCALLY instead (see there).
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
    // RC-1: reap the pane workers BEFORE tmux removes the session. tmux's
    // `kill-session` only delivers SIGHUP to pane processes; interactive
    // node-based CLIs (notably gemini) ignore SIGHUP and survive as orphans —
    // and because gemini re-execs its heavy child into its OWN process group, a
    // process-group kill misses it too. Walking the ppid tree and SIGKILLing
    // each pid reaps the worker regardless of its process group. Best-effort:
    // a missing tmux/ps or an already-dead pid is a harmless no-op.
    reap_pane_processes(name).await;
    // Reaping the pane shell can auto-destroy the now-empty session a beat
    // before this runs, so tmux replies "can't find session". Tolerate that
    // LOCALLY (not in the global tmux() helper — that would also silence
    // has-session/capture-pane against dead targets and break dead-session
    // detection in the daemon).
    match tmux(&["kill-session", "-t", name]).await {
        Ok(_) => Ok(()),
        Err(e) if e.contains("can't find") => Ok(()),
        Err(e) => Err(e),
    }
}

/// SIGKILL the full descendant process tree of every pane in session `name`.
async fn reap_pane_processes(name: &str) {
    // Pane pids across ALL windows of the session (`-s`).
    let out = match tmux(&["list-panes", "-s", "-t", name, "-F", "#{pane_pid}"]).await {
        Ok(o) => o,
        Err(_) => return,
    };
    let roots: Vec<i32> = out.lines().filter_map(|l| l.trim().parse().ok()).collect();
    if roots.is_empty() {
        return;
    }
    // (pid -> children) for the BFS, and (pid -> ppid) recorded from the SAME
    // snapshot so the kill step can revalidate identity (see below).
    let (children, parent) = ppid_snapshot().await;
    let victims = collect_descendants(&roots, &children);

    // PID-reuse defense: between the snapshot and the kill a victim pid could be
    // freed and recycled by an unrelated process, and a blind `kill -9` would
    // then hit the wrong target. So we (a) kill LEAVES-FIRST — reversing the
    // roots-first BFS order — which keeps every node's parent alive while we
    // validate it, and (b) SIGKILL a pid only if its CURRENT ppid still equals
    // the ppid we recorded in the snapshot. A recycled pid has a different
    // parent and is skipped; a vanished pid is a harmless no-op. pid<=1 is never
    // touched. This makes the reap reuse-safe rather than relying on a tiny
    // window staying tiny.
    for pid in victims.into_iter().rev() {
        if pid <= 1 {
            continue;
        }
        let Some(&recorded_ppid) = parent.get(&pid) else { continue };
        if current_ppid(pid).await == Some(recorded_ppid) {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }
}

/// Read a pid's CURRENT parent pid, or None if it is gone / unreadable.
async fn current_ppid(pid: i32) -> Option<i32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Snapshot the process table from `ps` as (pid -> children, pid -> ppid). Empty
/// on any failure, which makes the reap degrade to a harmless no-op.
async fn ppid_snapshot() -> (HashMap<i32, Vec<i32>>, HashMap<i32, i32>) {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut parent: HashMap<i32, i32> = HashMap::new();
    let out = match Command::new("ps").args(["-axo", "pid=,ppid="]).output().await {
        Ok(o) if o.status.success() => o.stdout,
        _ => return (children, parent),
    };
    for line in String::from_utf8_lossy(&out).lines() {
        let mut it = line.split_whitespace();
        if let (Some(p), Some(pp)) = (it.next(), it.next()) {
            if let (Ok(p), Ok(pp)) = (p.parse::<i32>(), pp.parse::<i32>()) {
                children.entry(pp).or_default().push(p);
                parent.insert(p, pp);
            }
        }
    }
    (children, parent)
}

/// BFS the ppid tree from `roots`, returning every reachable pid INCLUDING the
/// roots. Cycle-safe via a visited set. Pure (no I/O) so it is unit-testable.
/// A pid's process group is irrelevant here — that is the whole point: a gemini
/// child in its own pgid is still reached through parent linkage.
fn collect_descendants(roots: &[i32], children: &HashMap<i32, Vec<i32>>) -> Vec<i32> {
    let mut seen: HashSet<i32> = HashSet::new();
    let mut stack: Vec<i32> = roots.to_vec();
    let mut out = Vec::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        if let Some(cs) = children.get(&pid) {
            stack.extend(cs);
        }
    }
    out
}

pub async fn session_exists(name: &str) -> Result<bool, String> {
    // `has-session` exits 0 ONLY when the session exists. Check the raw exit
    // status directly — NOT via the tolerant tmux() helper, which maps BOTH a
    // present session (exit 0) AND "no server running" (the whole server is
    // down, so NO session exists) to Ok(""). Using `tmux().is_ok()` therefore
    // falsely reports exists=true once the last session is killed and the server
    // stops — which made the daemon's poll() never detect a dead session (zombie
    // in the map). A missing session ("can't find session") and a dead server
    // ("no server running") both correctly fail has-session → exists=false.
    let status = Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| format!("tmux has-session: {e}"))?;
    Ok(status.success())
}

pub async fn list_sessions() -> Result<Vec<String>, String> {
    let out = tmux(&["list-sessions", "-F", "#{session_name}"]).await?;
    Ok(out.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect())
}

#[cfg(test)]
mod reap_tests {
    use super::collect_descendants;
    use std::collections::HashMap;

    #[test]
    fn collects_full_tree_including_detached_pgid_child() {
        // 100 (pane shell) -> 200 (gemini parent) -> 300 (heap child, own pgid);
        // an unrelated tree under 999 must NOT be touched.
        let mut c: HashMap<i32, Vec<i32>> = HashMap::new();
        c.insert(100, vec![200]);
        c.insert(200, vec![300]);
        c.insert(999, vec![888]);
        let mut got = collect_descendants(&[100], &c);
        got.sort();
        assert_eq!(got, vec![100, 200, 300]);
    }

    #[test]
    fn multiple_roots_including_a_childless_root() {
        let mut c: HashMap<i32, Vec<i32>> = HashMap::new();
        c.insert(1, vec![2, 3]);
        let mut got = collect_descendants(&[1, 50], &c); // 50 has no children
        got.sort();
        assert_eq!(got, vec![1, 2, 3, 50]);
    }

    #[test]
    fn cycle_in_snapshot_terminates() {
        // Defensive: a malformed ps snapshot with a cycle must still terminate.
        let mut c: HashMap<i32, Vec<i32>> = HashMap::new();
        c.insert(1, vec![2]);
        c.insert(2, vec![1]);
        let got = collect_descendants(&[1], &c);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn empty_roots_yields_empty() {
        let c: HashMap<i32, Vec<i32>> = HashMap::new();
        assert!(collect_descendants(&[], &c).is_empty());
    }
}
