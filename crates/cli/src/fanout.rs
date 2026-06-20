use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::client;
use crate::spawn_session;

#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub prompt: String,
    /// Optional worker-shorthand pin ("kiro"/"gemini"/"claude"/"codex").
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Serialize)]
struct TaskResult {
    id: String,
    worker: String,
    profile: String,
    prompt: String,
    text: Option<String>,
    state: String,
    duration_secs: f64,
    events: serde_json::Value,
}

pub fn shorthand_to_profile(s: &str) -> Option<&'static str> {
    match s {
        "kiro" => Some("kiro-cli"),
        "gemini" => Some("gemini-cli"),
        "claude" => Some("claude-code"),
        "codex" => Some("codex-cli"),
        "grok" => Some("grok-cli"),
        "glm" => Some("glm-cli"),    // zai-org/GLM-5.2 via DeepInfra (headless-only)
        "kimi" => Some("kimi-cli"),  // moonshotai/Kimi-K2.7-Code via DeepInfra (headless-only)
        "deepseek" => Some("deepseek-cli"), // deepseek-ai/DeepSeek-V4-Pro via DeepInfra (headless-only)
        _ => None,
    }
}

pub fn parse_workers(spec: &str) -> Result<Vec<(String, u32)>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let (name, count) = part.split_once(':').ok_or_else(|| format!("bad worker spec '{part}' (want shorthand:N)"))?;
        let count: u32 = count.parse().map_err(|_| format!("bad worker count in '{part}'"))?;
        if count == 0 {
            return Err(format!("worker count must be >= 1 in '{part}'"));
        }
        if shorthand_to_profile(name).is_none() {
            return Err(format!("unknown worker shorthand '{name}' (kiro|gemini|claude|codex|grok|glm|kimi|deepseek)"));
        }
        out.push((name.to_string(), count));
    }
    Ok(out)
}

/// First task that is unpinned or pinned to this worker's shorthand.
pub fn pick_task(queue: &mut VecDeque<TaskSpec>, shorthand: &str) -> Option<TaskSpec> {
    let idx = queue.iter().position(|t| {
        t.profile.as_deref().map(|p| p == shorthand).unwrap_or(true)
    })?;
    queue.remove(idx)
}

async fn run_one_task(worker: &str, shorthand: &str, task: &TaskSpec, timeout: u64) -> TaskResult {
    let started = Instant::now();
    let profile = shorthand_to_profile(shorthand).unwrap_or("?").to_string();
    let fail = |state: &str, events: serde_json::Value, started: Instant| TaskResult {
        id: task.id.clone(), worker: worker.to_string(), profile: profile.clone(),
        prompt: task.prompt.clone(), text: None, state: state.to_string(),
        duration_secs: started.elapsed().as_secs_f64(), events,
    };

    if let Err(e) = client::request(json!({"send_async": {"session": worker, "text": task.prompt}})).await {
        return fail(&format!("error:{e}"), json!([]), started);
    }

    let mut answers = 0u32;
    loop {
        let remaining = timeout.saturating_sub(started.elapsed().as_secs()).max(1);
        let resp = match client::request(json!({"wait": {"session": worker, "timeout_secs": remaining}})).await {
            Ok(v) => v,
            Err(e) => return fail(&format!("error:{e}"), json!([]), started),
        };
        let state = resp.get("state").and_then(|s| s.as_str()).unwrap_or("error").to_string();
        let events = resp.get("events").cloned().unwrap_or_else(|| json!([]));
        match state.as_str() {
            "ready" => {
                let arr = events.as_array().cloned().unwrap_or_default();
                let text = client::last_assistant_text(&arr);
                return TaskResult {
                    id: task.id.clone(), worker: worker.to_string(), profile,
                    prompt: task.prompt.clone(), text, state: "ok".into(),
                    duration_secs: started.elapsed().as_secs_f64(), events,
                };
            }
            "question" if answers < 3 => {
                answers += 1;
                eprintln!("[fanout] {worker}/{}: auto-answering question ({answers}/3)", task.id);
                if let Err(e) = client::request(json!({"send_keys": {"session": worker, "keys": ["y", "Enter"]}})).await {
                    return fail(&format!("error:{e}"), events, started);
                }
            }
            "question" => return fail("failed:question_loop", events, started),
            "timeout" => {
                eprintln!("[fanout] {worker}/{}: timeout — interrupting", task.id);
                let _ = client::request(json!({"interrupt": worker})).await;
                let _ = client::request(json!({"wait": {"session": worker, "timeout_secs": 30}})).await;
                return fail("timeout", events, started);
            }
            other => return fail(other, events, started),
        }
    }
}

/// Map a (possibly hostile) task id to a safe single-component filename so it
/// can't escape out_dir. Non-[A-Za-z0-9._-] chars (incl. path separators) -> '_'.
fn result_path(out_dir: &Path, id: &str) -> std::path::PathBuf {
    let mut safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = "_".to_string();
    }
    // F4: sanitization is many-to-one — distinct ids ("a/b", "a:b", "a b") all map
    // to "a_b" and would silently overwrite each other's result file. When the id
    // had to be changed, disambiguate with a short stable hash of the ORIGINAL id.
    // Clean ids (unchanged by sanitization) keep their plain filename.
    if safe != id {
        out_dir.join(format!("{safe}-{}.json", short_hash(id)))
    } else {
        out_dir.join(format!("{safe}.json"))
    }
}

/// 8-hex-char stable hash (FNV-1a, folded to 32 bits) — deterministic across runs
/// (unlike DefaultHasher), used only to disambiguate sanitized result filenames.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

async fn worker_loop(
    worker: String,
    shorthand: String,
    queue: Arc<Mutex<VecDeque<TaskSpec>>>,
    out_dir: std::path::PathBuf,
    timeout: u64,
) -> Vec<TaskResult> {
    let mut results = Vec::new();
    loop {
        let task = { pick_task(&mut *queue.lock().await, &shorthand) };
        let Some(task) = task else { break };
        eprintln!("[fanout] {worker} -> {}", task.id);
        let result = run_one_task(&worker, &shorthand, &task, timeout).await;

        // Persist the result FIRST so even the respawn-failure early break
        // below leaves a per-task file on disk.
        let path = result_path(&out_dir, &result.id);
        if let Ok(jsonr) = serde_json::to_string_pretty(&result) {
            // F3: don't silently swallow the write failure — the .json is the only
            // home for the full text/events, and the summary would still count the
            // task as done.
            if let Err(e) = std::fs::write(&path, jsonr) {
                eprintln!("[fanout] WARNING: failed to write result {}: {e}", path.display());
            }
        }

        // A timed-out/dead worker gets recycled so the next task starts clean.
        if result.state == "timeout" || result.state == "dead" {
            eprintln!("[fanout] {worker}: recycling after {}", result.state);
            let _ = client::request(json!({"kill_session": worker})).await;
            let profile = shorthand_to_profile(&shorthand).unwrap_or("claude-code");
            if let Err(e) = spawn_session(&worker, profile, None, None).await {
                eprintln!("[fanout] {worker}: respawn failed, stopping worker: {e}");
                results.push(result);
                break;
            }
        }

        results.push(result);
    }
    results
}

use tmux_ai_parser::profile::{CompiledExec, CompiledProfile};

#[allow(clippy::too_many_arguments)] // a worker's full context; a struct would only obscure it
async fn exec_worker_loop(
    worker: String,
    shorthand: String,
    profile_name: String,
    cwd: String,
    exec: CompiledExec,
    queue: Arc<Mutex<VecDeque<TaskSpec>>>,
    out_dir: std::path::PathBuf,
    timeout: u64,
    retries: u32,
) -> Vec<TaskResult> {
    let mut results = Vec::new();
    loop {
        let task = { pick_task(&mut *queue.lock().await, &shorthand) };
        let Some(task) = task else { break };
        eprintln!("[fanout:exec] {worker} -> {}", task.id);
        let started = Instant::now();
        let r = crate::run::run_exec_retrying(&exec, &task.prompt, &cwd, timeout, false, retries).await;
        let state = if r.timed_out { "timeout" } else if r.ok { "ok" } else { "failed" };
        let result = TaskResult {
            id: task.id.clone(),
            worker: worker.clone(),
            profile: profile_name.clone(),
            prompt: task.prompt.clone(),
            text: r.answer.clone(),
            state: state.to_string(),
            duration_secs: started.elapsed().as_secs_f64(),
            events: json!({"raw": r.raw, "error": r.error, "exit_code": r.exit_code}),
        };
        let path = result_path(&out_dir, &result.id);
        if let Ok(j) = serde_json::to_string_pretty(&result) {
            if let Err(e) = std::fs::write(&path, j) {
                eprintln!("[fanout:exec] WARNING: failed to write result {}: {e}", path.display());
            }
        }
        results.push(result);
    }
    results
}

/// Daemon-less batch: a semaphore of N headless subprocesses per profile.
pub async fn run_exec_mode(
    workers_spec: &str,
    tasks_path: &Path,
    out_dir: &Path,
    timeout: u64,
    cwd: &str,
    retries: u32,
) -> Result<serde_json::Value, String> {
    let wall_start = Instant::now();
    let workers = parse_workers(workers_spec)?;
    let tasks: Vec<TaskSpec> = serde_json::from_str(
        &std::fs::read_to_string(tasks_path).map_err(|e| format!("read {}: {e}", tasks_path.display()))?,
    )
    .map_err(|e| format!("parse {}: {e}", tasks_path.display()))?;
    if tasks.is_empty() {
        return Err("task list is empty".into());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let mut loops = tokio::task::JoinSet::new();
    let mut worker_names: Vec<String> = Vec::new();
    for (shorthand, count) in &workers {
        let profile_name = shorthand_to_profile(shorthand).unwrap().to_string();
        let path = client::profiles_dir().join(format!("{profile_name}.toml"));
        let compiled = CompiledProfile::load(&path)?;
        let exec = compiled
            .exec
            .ok_or_else(|| format!("profile '{profile_name}' has no [exec] section"))?;
        for i in 1..=*count {
            let worker = format!("ex-{shorthand}-{i}");
            worker_names.push(worker.clone());
            loops.spawn(exec_worker_loop(
                worker,
                shorthand.clone(),
                profile_name.clone(),
                cwd.to_string(),
                exec.clone(),
                queue.clone(),
                out_dir.to_path_buf(),
                timeout,
                retries,
            ));
        }
    }

    let mut all: Vec<TaskResult> = Vec::new();
    while let Some(joined) = loops.join_next().await {
        // F1: a panicked worker must NOT abort the whole batch (which would drop
        // every other worker's completed results and cancel in-flight workers).
        // Log it and keep collecting the survivors.
        match joined {
            Ok(results) => all.extend(results),
            Err(e) => eprintln!("[fanout:exec] WARNING: a worker task panicked; its results are lost: {e}"),
        }
    }

    // Any tasks left in the queue were pinned to a profile that no provisioned
    // worker matched — surface them instead of silently under-executing.
    let skipped: Vec<String> = {
        let q = queue.lock().await;
        q.iter().map(|t| t.id.clone()).collect()
    };
    if !skipped.is_empty() {
        eprintln!(
            "[fanout:exec] WARNING: {} task(s) never ran (profile pin matched no provisioned worker): {}",
            skipped.len(), skipped.join(", ")
        );
    }

    let ok = all.iter().filter(|r| r.state == "ok").count();
    let summary = json!({
        "total": all.len(),
        "ok": ok,
        "failed": all.len() - ok,
        "wall_secs": wall_start.elapsed().as_secs_f64(),
        "mode": "exec",
        "workers": worker_names,
        "skipped": skipped,
    });
    let body = serde_json::to_string_pretty(&summary).map_err(|e| format!("serialize summary: {e}"))?;
    std::fs::write(out_dir.join("summary.json"), body).map_err(|e| format!("write summary: {e}"))?;
    Ok(summary)
}

pub async fn run(
    workers_spec: &str,
    tasks_path: &Path,
    out_dir: &Path,
    timeout: u64,
    keep: bool,
) -> Result<serde_json::Value, String> {
    let wall_start = Instant::now();
    let workers = parse_workers(workers_spec)?;
    let tasks: Vec<TaskSpec> = serde_json::from_str(
        &std::fs::read_to_string(tasks_path).map_err(|e| format!("read {}: {e}", tasks_path.display()))?
    ).map_err(|e| format!("parse {}: {e}", tasks_path.display()))?;
    if tasks.is_empty() {
        return Err("task list is empty".into());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    // Spawn the pool concurrently (daemon creates parallelize since the
    // 5s startup wait runs outside the manager lock).
    let mut names: Vec<(String, String)> = Vec::new(); // (worker, shorthand)
    let mut spawns = tokio::task::JoinSet::new();
    for (shorthand, count) in &workers {
        for i in 1..=*count {
            let worker = format!("wk-{shorthand}-{i}");
            let profile = shorthand_to_profile(shorthand).unwrap().to_string();
            names.push((worker.clone(), shorthand.clone()));
            spawns.spawn(async move {
                (worker.clone(), spawn_session(&worker, &profile, None, None).await)
            });
        }
    }
    let mut spawn_err: Option<String> = None;
    while let Some(joined) = spawns.join_next().await {
        match joined {
            Ok((worker, Ok(_))) => eprintln!("[fanout] spawned {worker}"),
            Ok((worker, Err(e))) => { spawn_err.get_or_insert(format!("spawn {worker}: {e}")); }
            Err(e) => { spawn_err.get_or_insert(format!("spawn task panicked: {e}")); }
        }
    }
    if let Some(e) = spawn_err {
        // F5: if any spawn failed, kill the workers that DID come up before
        // returning — otherwise they leak (the cleanup block below is unreached).
        for (worker, _) in &names {
            let _ = client::request(json!({"kill_session": worker})).await;
        }
        return Err(e);
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let mut loops = tokio::task::JoinSet::new();
    for (worker, shorthand) in names.clone() {
        loops.spawn(worker_loop(worker, shorthand, queue.clone(), out_dir.to_path_buf(), timeout));
    }
    let mut all: Vec<TaskResult> = Vec::new();
    while let Some(joined) = loops.join_next().await {
        // F1: a panicked worker must not abort the batch / drop other results.
        match joined {
            Ok(results) => all.extend(results),
            Err(e) => eprintln!("[fanout] WARNING: a worker task panicked; its results are lost: {e}"),
        }
    }

    if !keep {
        for (worker, _) in &names {
            let _ = client::request(json!({"kill_session": worker})).await;
        }
    }

    // F2: session mode, like exec mode, must report tasks left unrun (pinned to a
    // profile no provisioned worker matched) instead of silently under-executing.
    let skipped: Vec<String> = {
        let q = queue.lock().await;
        q.iter().map(|t| t.id.clone()).collect()
    };
    if !skipped.is_empty() {
        eprintln!(
            "[fanout] WARNING: {} task(s) never ran (profile pin matched no provisioned worker): {}",
            skipped.len(), skipped.join(", ")
        );
    }

    let ok = all.iter().filter(|r| r.state == "ok").count();
    let sum_durations: f64 = all.iter().map(|r| r.duration_secs).sum();
    let summary = json!({
        "total": all.len(),
        "ok": ok,
        "failed": all.len() - ok,
        "wall_secs": wall_start.elapsed().as_secs_f64(),
        "sum_task_secs": sum_durations,
        "workers": names.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>(),
        "kept": keep,
        "skipped": skipped,
    });
    // F8: surface a serialize failure instead of panicking (workers already wrote
    // their per-task files; the command should fail cleanly, not unwind).
    let body = serde_json::to_string_pretty(&summary).map_err(|e| format!("serialize summary: {e}"))?;
    std::fs::write(out_dir.join("summary.json"), body).map_err(|e| format!("write summary: {e}"))?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tmux_ai_parser::profile::{CompiledExec, ExecOutput};

    #[test]
    fn parses_worker_spec() {
        assert_eq!(
            parse_workers("kiro:2,gemini:1").unwrap(),
            vec![("kiro".to_string(), 2), ("gemini".to_string(), 1)]
        );
        assert!(parse_workers("kiro:0").is_err(), "zero workers rejected");
        assert!(parse_workers("unknown:1").is_err(), "unknown shorthand rejected");
    }

    #[test]
    fn maps_shorthands() {
        assert_eq!(shorthand_to_profile("kiro"), Some("kiro-cli"));
        assert_eq!(shorthand_to_profile("gemini"), Some("gemini-cli"));
        assert_eq!(shorthand_to_profile("claude"), Some("claude-code"));
        assert_eq!(shorthand_to_profile("grok"), Some("grok-cli"));
        assert_eq!(shorthand_to_profile("codex"), Some("codex-cli"));
        assert_eq!(shorthand_to_profile("nope"), None);
    }

    #[test]
    fn picks_compatible_tasks() {
        let mut q: VecDeque<TaskSpec> = VecDeque::from(vec![
            TaskSpec { id: "t1".into(), prompt: "a".into(), profile: Some("gemini".into()) },
            TaskSpec { id: "t2".into(), prompt: "b".into(), profile: None },
        ]);
        // kiro worker skips the gemini-pinned task, takes the unpinned one
        let picked = pick_task(&mut q, "kiro").unwrap();
        assert_eq!(picked.id, "t2");
        // gemini worker takes its pinned task
        let picked = pick_task(&mut q, "gemini").unwrap();
        assert_eq!(picked.id, "t1");
        assert!(pick_task(&mut q, "kiro").is_none());
    }

    #[tokio::test]
    async fn exec_worker_loop_runs_and_writes_results() {
        let dir = std::env::temp_dir().join(format!("tmuxai-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exec = CompiledExec {
            command: "echo {prompt}".into(),
            output: ExecOutput::Text,
            answer_path: String::new(),
            use_stdin: false,
        };
        let q = Arc::new(Mutex::new(VecDeque::from(vec![
            TaskSpec { id: "a".into(), prompt: "AA".into(), profile: None },
            TaskSpec { id: "b".into(), prompt: "BB".into(), profile: None },
        ])));
        let results = exec_worker_loop(
            "w1".into(), "grok".into(), "grok-cli".into(), ".".to_string(), exec, q, dir.clone(), 10, 0,
        ).await;
        assert_eq!(results.len(), 2);
        assert!(dir.join("a.json").exists());
        assert!(dir.join("b.json").exists());
        assert!(results.iter().all(|r| r.state == "ok"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn result_path_blocks_traversal() {
        let p = result_path(Path::new("/out"), "../etc/passwd");
        assert_eq!(p.parent(), Some(Path::new("/out")));
        // Normal ids are untouched (modulo the .json suffix).
        assert_eq!(result_path(Path::new("/out"), "task-1"), Path::new("/out/task-1.json"));
    }

    #[test]
    fn result_path_disambiguates_sanitized_collisions() {
        // F4: distinct ids that sanitize to the same name must NOT collide.
        let a = result_path(Path::new("/out"), "a/b");
        let b = result_path(Path::new("/out"), "a:b");
        assert_ne!(a, b, "distinct unsafe ids must map to distinct files");
        assert!(a.parent() == Some(Path::new("/out")) && b.parent() == Some(Path::new("/out")));
        // A clean id collides with neither (it keeps its plain name).
        assert_eq!(result_path(Path::new("/out"), "a_b"), Path::new("/out/a_b.json"));
        // Stable across calls.
        assert_eq!(result_path(Path::new("/out"), "a/b"), result_path(Path::new("/out"), "a/b"));
    }
}
