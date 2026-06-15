mod client;
mod fanout;
mod run;
mod watch;

use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use tmux_ai_parser::profile::CompiledProfile;

#[derive(Parser)]
#[command(name = "tmuxai", about = "Orchestrate AI CLI subagents in tmux", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Spawn a subagent session (command defaults to the profile's launch_command)
    Spawn {
        name: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        command: Option<String>,
    },
    /// Send a prompt. Sync by default (waits up to --timeout); --async returns immediately
    Send {
        name: String,
        text: String,
        #[arg(long = "async")]
        is_async: bool,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// One non-blocking status check
    Poll { name: String },
    /// Wait for one or more sessions to leave busy (exit 2 if any timed out)
    Wait {
        names: Vec<String>,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Print just the last assistant text. Refuses (exit 2) if the session is
    /// busy, so a freshly-sent prompt can't hand back the PREVIOUS answer; pass
    /// --stale to read the snapshot anyway.
    Text {
        name: String,
        #[arg(long)]
        stale: bool,
    },
    /// Show the pending question (text + choices) if the session is asking
    Question { name: String },
    /// Answer a permission question with raw keys, e.g.: tmuxai answer w1 y Enter
    Answer { name: String, keys: Vec<String> },
    /// List sessions
    Ls,
    /// Live dashboard of every session (state + last line). Runs until Ctrl-C;
    /// use --once for a single frame. Run it in a SEPARATE terminal/tmux pane,
    /// not the session driving the fleet.
    Watch {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Stream a session's raw pane log (the daemon's pipe-pane capture).
    /// -f follows like `tail -f`; -n sets how many trailing lines to seed.
    Logs {
        name: String,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// Kill one session
    Kill { name: String },
    /// Kill all sessions
    Killall,
    /// Fan a task list out over a worker pool
    Fanout {
        #[arg(long)]
        workers: String,
        #[arg(long)]
        tasks: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        #[arg(long)]
        keep: bool,
        #[arg(long, default_value = "session")]
        mode: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Run a one-shot headless prompt via the profile's [exec] mode (daemon-less).
    Run {
        profile: String,
        // FIX #3: allow prompts that start with `-` (e.g. bullet-list prompts).
        #[arg(allow_hyphen_values = true)]
        prompt: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 240)]
        timeout: u64,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        stdin: bool,
    },
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    // FIX #3: clap must accept a prompt that starts with `-`.
    #[test]
    fn run_accepts_hyphen_prefixed_prompt() {
        let cli = Cli::try_parse_from(["tmuxai", "run", "grok-cli", "- a bullet prompt"])
            .expect("hyphen prompt must parse");
        match cli.cmd {
            Cmd::Run { profile, prompt, .. } => {
                assert_eq!(profile, "grok-cli");
                assert_eq!(prompt, "- a bullet prompt");
            }
            _ => panic!("expected Run"),
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("[tmuxai] error: {msg}");
    std::process::exit(1);
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
}

/// Resolve the launch command for a profile shorthand-or-name.
pub fn launch_command_for(profile: &str) -> Result<String, String> {
    let path = client::profiles_dir().join(format!("{profile}.toml"));
    let compiled = CompiledProfile::load(&path)?;
    compiled.launch_command
        .ok_or_else(|| format!("profile '{profile}' has no launch_command; pass --command"))
}

pub async fn spawn_session(name: &str, profile: &str, cwd: Option<&str>, command: Option<&str>) -> Result<serde_json::Value, String> {
    let command = match command {
        Some(c) => c.to_string(),
        None => launch_command_for(profile)?,
    };
    let cwd = cwd.map(|s| s.to_string()).unwrap_or_else(|| {
        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into())
    });
    client::request(json!({"create_session": {
        "name": name, "command": command, "cwd": cwd, "profile": profile
    }})).await
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Spawn { name, profile, cwd, command } => {
            match spawn_session(&name, &profile, cwd.as_deref(), command.as_deref()).await {
                Ok(v) => print_json(&v),
                Err(e) => die(&e),
            }
        }
        Cmd::Send { name, text, is_async, timeout } => {
            if let Err(e) = client::request(json!({"send_async": {"session": name, "text": text}})).await {
                die(&e);
            }
            if is_async {
                print_json(&json!({"ok": true, "session": name}));
                return;
            }
            match client::request(json!({"wait": {"session": name, "timeout_secs": timeout}})).await {
                Ok(v) => {
                    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string();
                    print_json(&v);
                    std::process::exit(client::exit_code_for_state(&state));
                }
                Err(e) => die(&e),
            }
        }
        Cmd::Poll { name } => match client::request(json!({"poll": name})).await {
            Ok(v) => print_json(&v),
            Err(e) => die(&e),
        },
        Cmd::Wait { names, timeout } => {
            if names.is_empty() {
                die("wait needs at least one session name");
            }
            let mut set = tokio::task::JoinSet::new();
            for n in names {
                set.spawn(async move {
                    let r = client::request(json!({"wait": {"session": n, "timeout_secs": timeout}})).await;
                    (n, r)
                });
            }
            let mut map = serde_json::Map::new();
            let mut worst = 0;
            while let Some(joined) = set.join_next().await {
                let (n, r) = joined.unwrap_or_else(|e| ("<join>".into(), Err(e.to_string())));
                match r {
                    Ok(v) => {
                        let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("");
                        worst = worst.max(client::exit_code_for_state(state));
                        map.insert(n, v);
                    }
                    Err(e) => {
                        worst = worst.max(1);
                        map.insert(n, json!({"error": e}));
                    }
                }
            }
            print_json(&serde_json::Value::Object(map));
            std::process::exit(worst);
        }
        Cmd::Text { name, stale } => match client::request(json!({"poll": name})).await {
            Ok(v) => {
                let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("");
                if state == "busy" && !stale {
                    // Freshness guard: the worker hasn't finished this turn, so
                    // the snapshot still holds the previous answer. Don't hand
                    // back a stale read — wait for `ready` first (or --stale).
                    eprintln!("[tmuxai] session '{name}' is busy; answer not ready (use `wait` first, or --stale)");
                    std::process::exit(2);
                }
                let events = v.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default();
                match client::last_assistant_text(&events) {
                    Some(t) => println!("{t}"),
                    None => {
                        eprintln!("[tmuxai] no assistant text in current snapshot");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => die(&e),
        },
        Cmd::Question { name } => match client::request(json!({"poll": name})).await {
            Ok(v) => {
                // Same freshness guard as `text`: while busy the snapshot may
                // still hold the previous turn's question.
                if v.get("state").and_then(|s| s.as_str()) == Some("busy") {
                    eprintln!("[tmuxai] session '{name}' is busy; no current question (wait first)");
                    std::process::exit(2);
                }
                let events = v.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default();
                // The Question event carries the prompt text + choices even
                // under YOLO (auto-approve suppresses tool prompts, not
                // model-initiated questions).
                let q = events.iter().rev().find_map(|e| e.get("Question"));
                match q {
                    Some(question) => print_json(question),
                    None => {
                        eprintln!("[tmuxai] no pending question (state: {})",
                            v.get("state").and_then(|s| s.as_str()).unwrap_or("?"));
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => die(&e),
        },
        Cmd::Answer { name, keys } => {
            match client::request(json!({"send_keys": {"session": name, "keys": keys}})).await {
                Ok(v) => print_json(&v),
                Err(e) => die(&e),
            }
        }
        Cmd::Ls => match client::request(json!({"list_sessions": true})).await {
            Ok(v) => print_json(&v),
            Err(e) => die(&e),
        },
        Cmd::Watch { once, interval } => {
            if let Err(e) = watch::run(once, interval).await { die(&e); }
        }
        Cmd::Logs { name, follow, lines } => {
            if let Err(e) = watch::logs(&name, follow, lines).await { die(&e); }
        }
        Cmd::Kill { name } => match client::request(json!({"kill_session": name})).await {
            Ok(v) => print_json(&v),
            Err(e) => die(&e),
        },
        Cmd::Killall => match client::request(json!({"list_sessions": true})).await {
            Ok(v) => {
                let names: Vec<String> = v.get("sessions").and_then(|s| s.as_array()).map(|arr| {
                    arr.iter().filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from)).collect()
                }).unwrap_or_default();
                for n in &names {
                    let _ = client::request(json!({"kill_session": n})).await;
                }
                print_json(&json!({"ok": true, "killed": names}));
            }
            Err(e) => die(&e),
        },
        Cmd::Fanout { workers, tasks, out, timeout, keep, mode, cwd } => {
            let result = match mode.as_str() {
                "session" => fanout::run(&workers, &tasks, &out, timeout, keep).await,
                "exec" => {
                    let cwd = cwd.unwrap_or_else(|| {
                        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into())
                    });
                    fanout::run_exec_mode(&workers, &tasks, &out, timeout, &cwd).await
                }
                other => Err(format!("--mode must be 'session' or 'exec', got '{other}'")),
            };
            match result {
                Ok(v) => print_json(&v),
                Err(e) => die(&e),
            }
        }
        Cmd::Run { profile, prompt, cwd, timeout, json: as_json, stdin } => {
            let path = client::profiles_dir().join(format!("{profile}.toml"));
            let compiled = match CompiledProfile::load(&path) {
                Ok(c) => c,
                Err(e) => die(&e),
            };
            let exec = match compiled.exec.as_ref() {
                Some(e) => e,
                None => die(&format!(
                    "profile '{profile}' has no [exec] section; use spawn/send for session mode"
                )),
            };
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into())
            });
            let started = std::time::Instant::now();
            let res = run::run_exec(exec, &prompt, &cwd, timeout, stdin).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            if as_json {
                print_json(&json!({
                    "ok": res.ok, "profile": profile, "mode": "exec",
                    "answer": res.answer, "raw": res.raw,
                    "exit_code": res.exit_code, "timed_out": res.timed_out,
                    "duration_ms": duration_ms, "error": res.error,
                }));
            } else if let Some(a) = &res.answer {
                println!("{a}");
            } else if let Some(e) = &res.error {
                eprintln!("[tmuxai] run failed: {e}");
            }
            let code = if res.timed_out { 2 } else if res.ok { 0 } else { 1 };
            std::process::exit(code);
        }
    }
}
