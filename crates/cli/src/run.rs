use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tmux_ai_parser::profile::CompiledExec;
use tmux_ai_parser::profile::ExecOutput;

#[derive(Debug)]
pub struct RunResult {
    pub ok: bool,
    pub answer: Option<String>,
    pub raw: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub error: Option<String>,
}

/// Build argv from a command template by whitespace-splitting and substituting
/// `{prompt}` with the prompt as ONE element (no shell — no quoting needed).
/// When `use_stdin` is true the `{prompt}` placeholder is dropped.
pub fn build_argv(command: &str, prompt: &str, use_stdin: bool) -> Vec<String> {
    let mut argv = Vec::new();
    for tok in command.split_whitespace() {
        if tok == "{prompt}" {
            if !use_stdin {
                argv.push(prompt.to_string());
            }
        } else {
            argv.push(tok.to_string());
        }
    }
    argv
}

/// "messages[-1]" -> ("messages", Some(-1)); "response" -> ("response", None).
fn split_index(seg: &str) -> (&str, Option<i64>) {
    if let Some(open) = seg.find('[') {
        if seg.ends_with(']') {
            let key = &seg[..open];
            let num = &seg[open + 1..seg.len() - 1];
            if let Ok(i) = num.parse::<i64>() {
                return (key, Some(i));
            }
        }
    }
    (seg, None)
}

/// Minimal jq-style extractor: leading `.`, dot-separated keys, optional `[n]`
/// (negative = from end) per segment. Returns string values raw, others as JSON.
pub fn extract_path(v: &serde_json::Value, path: &str) -> Option<String> {
    let mut cur = v;
    let trimmed = path.trim_start_matches('.');
    if !trimmed.is_empty() {
        for seg in trimmed.split('.') {
            let (key, idx) = split_index(seg);
            if !key.is_empty() {
                cur = cur.get(key)?;
            }
            if let Some(i) = idx {
                let arr = cur.as_array()?;
                let real = if i < 0 { arr.len() as i64 + i } else { i };
                if real < 0 {
                    return None;
                }
                cur = arr.get(real as usize)?;
            }
        }
    }
    match cur {
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Strip raw ASCII control characters (0x00–0x1F + DEL) so a JSON payload whose
/// string values contain unescaped control chars (e.g. grok-build's thinking
/// trace) still parses with strict serde. Used only as PARSE input — `raw`
/// keeps the original bytes. Multibyte (UTF-8) content is preserved.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_control()).collect()
}

/// Strip ANSI/CSI escape sequences (color codes, etc.) from CLI output so a
/// text-mode answer is plain text. Manual scan — no regex dependency. Vendors
/// that emit no escapes (e.g. codex exec) are unaffected.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI sequence: ESC '[' ... <final ASCII letter>
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // lone ESC is dropped
        } else {
            out.push(c);
        }
    }
    out
}

/// Turn captured stdout/exit into the normalized RunResult.
pub fn normalize(
    output: &ExecOutput,
    answer_path: &str,
    stdout: &str,
    success: bool,
    code: i32,
    stderr: &str,
) -> RunResult {
    if !success {
        return RunResult {
            ok: false,
            answer: None,
            raw: stdout.to_string(),
            exit_code: code,
            timed_out: false,
            error: Some(format!("non-zero exit ({code}): {}", stderr.trim())),
        };
    }
    // RC-3: a clean exit with empty/whitespace-only stdout is a SILENT failure
    // (e.g. grok-build occasionally exits 0 producing nothing, or gemini's
    // trust-downgrade returns blank). Text mode would otherwise hand back an
    // empty answer with ok:true; classify it as a retryable failure instead so
    // the caller never mistakes a blank for a real response.
    if stdout.trim().is_empty() {
        return RunResult {
            ok: false,
            answer: None,
            raw: stdout.to_string(),
            exit_code: code,
            timed_out: false,
            error: Some("empty output (exit 0, no stdout)".into()),
        };
    }
    match output {
        ExecOutput::Text => RunResult {
            ok: true,
            answer: Some(strip_ansi(stdout).trim().to_string()),
            raw: stdout.to_string(),
            exit_code: code,
            timed_out: false,
            error: None,
        },
        ExecOutput::Json => match serde_json::from_str::<serde_json::Value>(&strip_control_chars(stdout.trim())) {
            Ok(v) => match extract_path(&v, answer_path) {
                Some(a) => RunResult {
                    ok: true,
                    answer: Some(a),
                    raw: stdout.to_string(),
                    exit_code: code,
                    timed_out: false,
                    error: None,
                },
                None => RunResult {
                    ok: false,
                    answer: None,
                    raw: stdout.to_string(),
                    exit_code: code,
                    timed_out: false,
                    error: Some(format!("answer_path '{answer_path}' not found")),
                },
            },
            Err(e) => RunResult {
                ok: false,
                answer: None,
                raw: stdout.to_string(),
                exit_code: code,
                timed_out: false,
                error: Some(format!("JSON parse failed: {e}")),
            },
        },
    }
}

/// Spawn the profile's headless command (no shell, no daemon), enforce a timeout,
/// and normalize the output. `kill_on_drop` ensures a timed-out child is reaped.
pub async fn run_exec(
    exec: &CompiledExec,
    prompt: &str,
    cwd: &str,
    timeout_secs: u64,
    stdin_override: bool,
) -> RunResult {
    let use_stdin = exec.use_stdin || stdin_override;
    let argv = build_argv(&exec.command, prompt, use_stdin);
    if argv.is_empty() {
        return RunResult {
            ok: false, answer: None, raw: String::new(), exit_code: -1,
            timed_out: false, error: Some("empty exec command".into()),
        };
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if use_stdin { Stdio::piped() } else { Stdio::null() })
        .kill_on_drop(true);

    // FIX #1: Place the child in its own process group so that on timeout we
    // can signal the entire group (killing grandchildren like sub-agents too).
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunResult {
                ok: false, answer: None, raw: String::new(), exit_code: -1,
                timed_out: false, error: Some(format!("spawn {}: {e}", argv[0])),
            }
        }
    };

    // Capture pid BEFORE moving child into the async block (FIX #1).
    // Only used in the #[cfg(unix)] killpg block, so cfg-gate to avoid an
    // unused-variable warning on non-unix builds.
    #[cfg(unix)]
    let pid = child.id();

    // FIX #2: Wrap the stdin write INSIDE the timeout-bounded future so a
    // slow/large-prompt drain cannot block past --timeout with timed_out:false.
    let io = async move {
        if use_stdin {
            if let Some(mut sin) = child.stdin.take() {
                let _ = sin.write_all(prompt.as_bytes()).await;
                // sin drops here -> stdin EOF
            }
        }
        child.wait_with_output().await
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), io).await {
        Err(_) => {
            // FIX #1: Kill the entire process group, not just the direct child.
            #[cfg(unix)]
            if let Some(p) = pid {
                // SAFETY / PID-reuse: killing the negative pgid is safe from PID reuse here.
                // POSIX guarantees a PID is not reused while a process group with that ID still
                // exists; the child is its own group leader (process_group(0) => pgid == pid) and
                // the grandchildren we're reaping keep that group — and thus the pid number —
                // reserved. We also run this inline right after the timeout fires, before tokio's
                // async orphan reaper. If the whole group already exited, kill(-pid) is a harmless
                // ESRCH no-op.
                let rc = unsafe { libc::kill(-(p as i32), libc::SIGKILL) };
                if rc != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::ESRCH) {
                        eprintln!("[tmuxai] warning: failed to kill process group {p}: {e}");
                    }
                }
            }
            RunResult {
                ok: false, answer: None, raw: String::new(), exit_code: -1,
                timed_out: true, error: Some(format!("timed out after {timeout_secs}s")),
            }
        },
        Ok(Err(e)) => RunResult {
            ok: false, answer: None, raw: String::new(), exit_code: -1,
            timed_out: false, error: Some(format!("wait failed: {e}")),
        },
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            normalize(&exec.output, &exec.answer_path, &stdout, output.status.success(), code, &stderr)
        }
    }
}

/// RC-2: retry wrapper. Run the headless command up to `1 + retries` times,
/// retrying while the result is not ok — this absorbs the transient,
/// non-deterministic failures the fleet actually hits: grok-build's intermittent
/// `--single` arg error or empty exit, and an occasional gemini hang→timeout that
/// succeeds on a second try (steady-state gemini answers in ~9s). Deterministic
/// failures (bad answer_path, missing binary) simply exhaust the small budget,
/// adding only a couple of fast-failing attempts. A short escalating backoff
/// separates attempts. `retries == 0` reproduces the old single-shot behavior.
pub async fn run_exec_retrying(
    exec: &CompiledExec,
    prompt: &str,
    cwd: &str,
    timeout_secs: u64,
    stdin_override: bool,
    retries: u32,
) -> RunResult {
    let mut last = run_exec(exec, prompt, cwd, timeout_secs, stdin_override).await;
    let mut attempt = 0u32;
    while !last.ok && attempt < retries {
        let backoff = Duration::from_millis(400 * (attempt as u64 + 1));
        eprintln!(
            "[tmuxai] exec attempt {}/{} failed ({}); retrying in {}ms",
            attempt + 1,
            retries + 1,
            last.error.as_deref().unwrap_or("?"),
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        last = run_exec(exec, prompt, cwd, timeout_secs, stdin_override).await;
        attempt += 1;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_exec(command: &str, output: ExecOutput, answer_path: &str) -> CompiledExec {
        CompiledExec {
            command: command.to_string(),
            output,
            answer_path: answer_path.to_string(),
            use_stdin: false,
        }
    }

    #[tokio::test]
    async fn run_exec_echo_text_ok() {
        let exec = echo_exec("echo {prompt}", ExecOutput::Text, "");
        let r = run_exec(&exec, "hello world", ".", 10, false).await;
        assert!(r.ok, "error: {:?}", r.error);
        assert_eq!(r.answer.as_deref(), Some("hello world"));
        assert!(!r.timed_out);
    }

    #[tokio::test]
    async fn run_exec_nonzero_is_error() {
        let exec = echo_exec("false", ExecOutput::Text, "");
        let r = run_exec(&exec, "x", ".", 10, false).await;
        assert!(!r.ok);
        assert!(!r.timed_out);
        assert_eq!(r.exit_code, 1);
    }

    #[tokio::test]
    async fn run_exec_times_out() {
        let exec = echo_exec("sleep 5", ExecOutput::Text, "");
        let r = run_exec(&exec, "x", ".", 1, false).await;
        assert!(r.timed_out);
        assert!(!r.ok);
    }

    #[test]
    fn build_argv_substitutes_prompt_as_single_arg() {
        let a = build_argv("grok -p {prompt} --output-format json", "two words", false);
        assert_eq!(a, vec!["grok", "-p", "two words", "--output-format", "json"]);
    }

    #[test]
    fn build_argv_stdin_drops_placeholder() {
        let a = build_argv("codex-fleet exec {prompt}", "hi", true);
        assert_eq!(a, vec!["codex-fleet", "exec"]);
    }

    #[test]
    fn extract_path_simple_key() {
        let v = serde_json::json!({"response": "hi"});
        assert_eq!(extract_path(&v, ".response"), Some("hi".to_string()));
    }

    #[test]
    fn extract_path_nested_and_negative_index() {
        let v = serde_json::json!({"messages": [{"content": "a"}, {"content": "b"}]});
        assert_eq!(extract_path(&v, ".messages[-1].content"), Some("b".to_string()));
    }

    #[test]
    fn extract_path_missing_is_none() {
        let v = serde_json::json!({"a": 1});
        assert_eq!(extract_path(&v, ".nope"), None);
    }

    #[test]
    fn normalize_text_trims() {
        let r = normalize(&ExecOutput::Text, "", "  hi\n", true, 0, "");
        assert!(r.ok);
        assert_eq!(r.answer.as_deref(), Some("hi"));
    }

    #[test]
    fn normalize_json_extracts() {
        let r = normalize(&ExecOutput::Json, ".response", "{\"response\":\"hi\"}", true, 0, "");
        assert_eq!(r.answer.as_deref(), Some("hi"));
    }

    #[test]
    fn normalize_json_malformed_errors() {
        let r = normalize(&ExecOutput::Json, ".response", "not json", true, 0, "");
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("JSON parse"));
    }

    #[test]
    fn normalize_nonzero_errors() {
        let r = normalize(&ExecOutput::Text, "", "", false, 2, "boom");
        assert!(!r.ok);
        assert_eq!(r.exit_code, 2);
    }

    #[test]
    fn normalize_json_tolerates_raw_control_chars() {
        // grok-build's thinking trace embeds raw control chars in string values,
        // which strict serde rejects; we strip them before parsing.
        let bad = "{\"text\":\"hello\",\"thought\":\"a\x01b\nc\"}";
        let r = normalize(&ExecOutput::Json, ".text", bad, true, 0, "");
        assert!(r.ok, "should parse after control-char strip; error: {:?}", r.error);
        assert_eq!(r.answer.as_deref(), Some("hello"));
        // raw is preserved (still contains the original bytes)
        assert!(r.raw.contains("thought"));
    }

    #[test]
    fn strip_ansi_removes_csi_keeps_text() {
        assert_eq!(strip_ansi("\u{1b}[0mhi\u{1b}[38;5;1mthere"), "hithere");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn normalize_text_strips_ansi() {
        // kiro emits ANSI color codes + a "> " prompt marker; the ANSI is stripped
        // (the "> " is kept — a leading "> " can be legitimate review content).
        let r = normalize(&ExecOutput::Text, "", "\u{1b}[38;5;141m> \u{1b}[0mPING", true, 0, "");
        assert_eq!(r.answer.as_deref(), Some("> PING"));
    }

    // FIX #2: stdin write is now inside the timeout boundary; cat echoes it back.
    #[tokio::test]
    async fn run_exec_stdin_path_roundtrips() {
        let exec = CompiledExec {
            command: "cat".into(),
            output: ExecOutput::Text,
            answer_path: String::new(),
            use_stdin: true,
        };
        let r = run_exec(&exec, "hello-stdin", ".", 10, false).await;
        assert!(r.ok, "error: {:?}", r.error);
        assert_eq!(r.answer.as_deref(), Some("hello-stdin"));
    }

    // FIX #1: On timeout, the entire process group (including grandchildren) is
    // killed, not just the direct child.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_exec_timeout_kills_grandchild_process_group() {
        let dir = std::env::temp_dir().join(format!("tmuxai-pg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("gc.pid");
        let script = dir.join("spawn.sh");
        // The script backgrounds a long sleep, records its PID, then waits so it
        // doesn't exit before we have a chance to read the file.
        std::fs::write(
            &script,
            format!("sleep 30 & echo $! > {}; wait\n", pidfile.display()),
        )
        .unwrap();
        let exec = CompiledExec {
            command: format!("sh {}", script.display()),
            output: ExecOutput::Text,
            answer_path: String::new(),
            use_stdin: false,
        };
        let r = run_exec(&exec, "", ".", 1, false).await;
        assert!(r.timed_out, "expected timed_out but got: {:?}", r);
        let gc_raw = std::fs::read_to_string(&pidfile)
            .expect("grandchild pid file was never written — script didn't run");
        let gc: i32 = gc_raw.trim().parse().unwrap();
        // Poll for the grandchild's death (de-flaked: no fixed sleep). kill(pid, 0)
        // returns 0 while the process is alive, non-zero once it's gone.
        let mut dead = false;
        for _ in 0..30 {
            // up to ~3s
            if unsafe { libc::kill(gc, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(dead, "grandchild {gc} survived the timeout — process group not killed");
        std::fs::remove_dir_all(&dir).ok();
    }

    // RC-3: a clean exit producing only whitespace is a failure, not ok:true.
    #[test]
    fn normalize_empty_output_is_failure() {
        let r = normalize(&ExecOutput::Text, "", "   \n", true, 0, "");
        assert!(!r.ok, "empty/whitespace output must be a failure");
        assert!(r.error.unwrap().contains("empty output"));
    }

    #[tokio::test]
    async fn run_exec_empty_stdout_is_failure() {
        // `true` exits 0 with no stdout — previously returned ok:true with a blank answer.
        let exec = echo_exec("true", ExecOutput::Text, "");
        let r = run_exec(&exec, "", ".", 10, false).await;
        assert!(!r.ok, "exit 0 + empty stdout must fail; error: {:?}", r.error);
    }

    // RC-2: a transient failure (fails twice, succeeds on the 3rd run) is
    // absorbed by the retry budget. The counter file proves exactly 3 attempts ran.
    #[tokio::test]
    async fn run_exec_retrying_recovers_after_transient_failures() {
        let dir = std::env::temp_dir().join(format!("tmuxai-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("n");
        let script = dir.join("flaky.sh");
        std::fs::write(&counter, "0").unwrap();
        std::fs::write(
            &script,
            format!(
                "n=$(cat {c}); n=$((n+1)); echo $n > {c}; if [ $n -ge 3 ]; then echo OK; else exit 1; fi\n",
                c = counter.display()
            ),
        )
        .unwrap();
        let exec = CompiledExec {
            command: format!("sh {}", script.display()),
            output: ExecOutput::Text,
            answer_path: String::new(),
            use_stdin: false,
        };
        let r = run_exec_retrying(&exec, "", ".", 10, false, 2).await;
        assert!(r.ok, "should recover by attempt 3; error: {:?}", r.error);
        assert_eq!(r.answer.as_deref(), Some("OK"));
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().trim(),
            "3",
            "expected exactly 3 attempts (1 + 2 retries)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // RC-2: a deterministic failure exhausts the budget and still reports !ok.
    #[tokio::test]
    async fn run_exec_retrying_exhausts_then_fails() {
        let exec = echo_exec("false", ExecOutput::Text, "");
        let r = run_exec_retrying(&exec, "", ".", 10, false, 2).await;
        assert!(!r.ok);
    }

    // RC-2: retries == 0 is exactly one attempt (old single-shot behavior).
    #[tokio::test]
    async fn run_exec_retrying_zero_is_single_shot() {
        let exec = echo_exec("echo {prompt}", ExecOutput::Text, "");
        let r = run_exec_retrying(&exec, "hi", ".", 10, false, 0).await;
        assert!(r.ok);
        assert_eq!(r.answer.as_deref(), Some("hi"));
    }
}
