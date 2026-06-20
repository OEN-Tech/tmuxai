//! `tmuxai fleet-status` — probe each fleet member's live answerability.
//!
//! None of the underlying CLIs (codex/grok/gemini/kiro) expose a "remaining
//! quota" command, so the only reliable signal is whether a member can answer a
//! trivial prompt RIGHT NOW. This fires a 1-token prompt at each member via the
//! headless exec path (no daemon, no retries) concurrently and classifies the
//! result. When a member fails, we best-effort classify WHY from the error text
//! (rate-limited / quota-exhausted / auth) — the vendors don't report quota
//! proactively, so a non-`ok` reason is a heuristic, not a guarantee.

use crate::client;
use crate::fanout::shorthand_to_profile;
use serde_json::{json, Value};
use std::time::Instant;
use tmux_ai_parser::profile::CompiledProfile;

/// Members probed when `--workers` is omitted (claude-code has no [exec] section).
const DEFAULT_WORKERS: [&str; 4] = ["grok", "gemini", "codex", "kiro"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Ok,
    RateLimited,
    QuotaExhausted,
    AuthFailed,
    Timeout,
    Down,
}

impl ProbeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Ok => "ok",
            ProbeStatus::RateLimited => "rate_limited",
            ProbeStatus::QuotaExhausted => "quota_exhausted",
            ProbeStatus::AuthFailed => "auth_failed",
            ProbeStatus::Timeout => "timeout",
            ProbeStatus::Down => "down",
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            ProbeStatus::Ok => "✓",
            ProbeStatus::RateLimited => "⏳",
            ProbeStatus::QuotaExhausted => "∅",
            ProbeStatus::AuthFailed => "🔒",
            ProbeStatus::Timeout => "⌛",
            ProbeStatus::Down => "✗",
        }
    }
}

/// Pure classifier — maps a probe outcome to a status + short reason. Kept free
/// of I/O so the (heuristic, vendor-specific) signatures are unit-testable.
pub fn classify(ok: bool, timed_out: bool, answer: Option<&str>, error: Option<&str>) -> (ProbeStatus, String) {
    if timed_out {
        return (ProbeStatus::Timeout, "no response before timeout".to_string());
    }
    if ok && answer.map(|a| !a.trim().is_empty()).unwrap_or(false) {
        return (ProbeStatus::Ok, String::new());
    }
    let raw = error.unwrap_or("").trim();
    let e = raw.to_lowercase();

    // Order matters: rate-limit (transient, resets) before quota (exhausted) before auth.
    // F9: NO bare numeric HTTP codes ("429"/"401") — they substring-match unrelated
    // text (a path "/tmp/429.log", "exit code 1429", a request id) and misclassify
    // a plain failure as rate_limited/auth_failed. Real rate/auth errors carry
    // textual phrasing ("429 Too Many Requests" contains "too many requests";
    // "401 Unauthorized" contains "unauthorized"), which these phrases still catch.
    const RATE: [&str; 7] = [
        "rate limit", "rate_limit", "ratelimit", "too many requests",
        "throttl", "try again later", "overloaded",
    ];
    const QUOTA: [&str; 9] = [
        "resource_exhausted", "quota", "insufficient credit", "out of credit",
        "usage limit", "weekly limit", "daily limit", "credit balance", "exceeded your",
    ];
    // NB: grok ALWAYS prints a benign "Auth(AuthorizationRequired)" stderr line
    // from a secondary worker even on success — so that bare token is NOT an auth
    // signal. Require a clearer "not signed in / unauthorized / expired" phrasing.
    const AUTH: [&str; 8] = [
        "unauthorized", "not logged in", "please log in", "please login",
        "authentication failed", "sign in to", "session expired", "logged out",
    ];

    if raw.is_empty() {
        return (ProbeStatus::Down, "empty output / no error text".to_string());
    }

    // QUOTA before RATE: a "429 RESOURCE_EXHAUSTED / quota" (gemini's
    // quota-exceeded) is more specific than the bare 429 rate code; a pure rate
    // limit ("rate limit reached", "overloaded") carries no quota words and falls
    // through to RATE. The reason is the line that actually MATCHED (not the
    // first line, which is often a startup banner like gemini's "YOLO mode …").
    if let Some(n) = QUOTA.iter().find(|n| e.contains(**n)) {
        (ProbeStatus::QuotaExhausted, best_line(raw, n))
    } else if let Some(n) = RATE.iter().find(|n| e.contains(**n)) {
        (ProbeStatus::RateLimited, best_line(raw, n))
    } else if let Some(n) = AUTH.iter().find(|n| e.contains(**n)) {
        (ProbeStatus::AuthFailed, best_line(raw, n))
    } else {
        // No signature: show the first line that isn't a startup banner.
        (ProbeStatus::Down, first_meaningful_line(raw))
    }
}

/// Pick the most informative line containing `needle`: prefer one without a file
/// path / stack frame (the clean human message), trimmed to a short snippet.
fn best_line(raw: &str, needle: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| l.to_lowercase().contains(needle)).collect();
    let pick = lines
        .iter()
        .find(|l| !l.contains('/') && !l.trim_start().starts_with("at "))
        .or_else(|| lines.first())
        .copied()
        .unwrap_or_else(|| raw.lines().next().unwrap_or(raw));
    snippet(pick)
}

/// First line that isn't an obvious startup banner / approval notice.
fn first_meaningful_line(raw: &str) -> String {
    let banner = ["yolo mode", "ripgrep is not available", "skill \"", "all tool calls will be"];
    let pick = raw
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !banner.iter().any(|b| l.to_lowercase().contains(b)))
        .unwrap_or_else(|| raw.lines().next().unwrap_or(raw).trim());
    snippet(pick)
}

fn snippet(line: &str) -> String {
    line.trim()
        .trim_start_matches(['"', ',', ' '])
        .chars()
        .take(160)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Resolve which profiles to probe: `--workers grok,codex` (shorthands) or the
/// default four. Only members with an [exec] section are probe-able.
fn resolve_workers(spec: Option<&str>) -> Result<Vec<String>, String> {
    let shorthands: Vec<String> = match spec {
        Some(s) => s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
        None => DEFAULT_WORKERS.iter().map(|s| s.to_string()).collect(),
    };
    let mut profiles = Vec::new();
    for sh in shorthands {
        let name = shorthand_to_profile(&sh)
            .ok_or_else(|| format!("unknown worker '{sh}' (grok|gemini|codex|kiro|claude)"))?;
        profiles.push(name.to_string());
    }
    Ok(profiles)
}

/// Probe one member: load its [exec], run the prompt once (no retries), classify.
async fn probe(profile_name: String, prompt: String, cwd: String, timeout: u64) -> Value {
    let path = client::profiles_dir().join(format!("{profile_name}.toml"));
    let compiled = match CompiledProfile::load(&path) {
        Ok(c) => c,
        Err(e) => {
            return json!({"profile": profile_name, "status": ProbeStatus::Down.as_str(),
                          "icon": ProbeStatus::Down.icon(), "detail": format!("profile load failed: {e}"),
                          "latency_ms": 0});
        }
    };
    let Some(exec) = compiled.exec.as_ref() else {
        return json!({"profile": profile_name, "status": ProbeStatus::Down.as_str(),
                      "icon": ProbeStatus::Down.icon(),
                      "detail": "no [exec] section (not probe-able headless)", "latency_ms": 0});
    };
    let started = Instant::now();
    let r = crate::run::run_exec_retrying(exec, &prompt, &cwd, timeout, false, 0).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let (status, reason) = classify(r.ok, r.timed_out, r.answer.as_deref(), r.error.as_deref());
    json!({
        "profile": profile_name,
        "status": status.as_str(),
        "icon": status.icon(),
        "detail": reason,
        "latency_ms": latency_ms,
        "answer": r.answer,
    })
}

/// Run the fleet status probe across all selected members concurrently.
pub async fn run(
    workers_spec: Option<&str>,
    timeout: u64,
    prompt: &str,
    cwd: &str,
) -> Result<Value, String> {
    let profiles = resolve_workers(workers_spec)?;
    let mut set = tokio::task::JoinSet::new();
    for name in profiles {
        let (p, c) = (prompt.to_string(), cwd.to_string());
        set.spawn(async move { probe(name, p, c, timeout).await });
    }
    let mut results: Vec<Value> = Vec::new();
    while let Some(joined) = set.join_next().await {
        // F1: a panicked probe must not abort the whole status report — skip it
        // and still show the other members.
        match joined {
            Ok(v) => results.push(v),
            Err(e) => eprintln!("[fleet-status] WARNING: a probe task panicked: {e}"),
        }
    }
    // Stable order by the default ranking, then name.
    results.sort_by_key(|v| {
        let name = v.get("profile").and_then(|p| p.as_str()).unwrap_or("");
        DEFAULT_WORKERS
            .iter()
            .position(|w| shorthand_to_profile(w) == Some(name))
            .unwrap_or(usize::MAX)
    });
    let ok = results.iter().filter(|v| v.get("status").and_then(|s| s.as_str()) == Some("ok")).count();
    Ok(json!({ "ok": ok, "total": results.len(), "members": results }))
}

/// Human-readable table for non-JSON output.
pub fn render_table(summary: &Value) {
    let members = summary.get("members").and_then(|m| m.as_array()).cloned().unwrap_or_default();
    println!("  FLEET      STATUS            LATENCY  DETAIL");
    println!("  ──────────────────────────────────────────────────────────────────────");
    for m in &members {
        let p = m.get("profile").and_then(|v| v.as_str()).unwrap_or("?");
        let icon = m.get("icon").and_then(|v| v.as_str()).unwrap_or(" ");
        let st = m.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let lat = m.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let detail = m.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        println!("  {p:<10} {icon} {st:<15} {lat:>5}ms  {detail}");
    }
    let ok = summary.get("ok").and_then(|v| v.as_u64()).unwrap_or(0);
    let total = summary.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("  ──────────────────────────────────────────────────────────────────────");
    println!("  {ok}/{total} answerable");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_when_answer_present() {
        let (s, _) = classify(true, false, Some("OK"), None);
        assert_eq!(s, ProbeStatus::Ok);
    }

    #[test]
    fn empty_answer_is_not_ok() {
        let (s, _) = classify(true, false, Some("  "), None);
        assert_ne!(s, ProbeStatus::Ok);
    }

    #[test]
    fn timeout_classified() {
        let (s, _) = classify(false, true, None, Some("timed out after 10s"));
        assert_eq!(s, ProbeStatus::Timeout);
    }

    #[test]
    fn rate_limit_signatures() {
        for e in ["Error: 429 Too Many Requests", "rate limit reached, resets at 3pm",
                  "model is overloaded, throttled"] {
            let (s, _) = classify(false, false, None, Some(e));
            assert_eq!(s, ProbeStatus::RateLimited, "for {e:?}");
        }
    }

    #[test]
    fn quota_signatures() {
        for e in ["429 RESOURCE_EXHAUSTED: quota exceeded", "you have exceeded your daily limit",
                  "insufficient credit balance"] {
            let (s, _) = classify(false, false, None, Some(e));
            assert_eq!(s, ProbeStatus::QuotaExhausted, "for {e:?}");
        }
    }

    #[test]
    fn auth_signatures() {
        for e in ["401 Unauthorized", "you are not logged in", "session expired, please login"] {
            let (s, _) = classify(false, false, None, Some(e));
            assert_eq!(s, ProbeStatus::AuthFailed, "for {e:?}");
        }
    }

    #[test]
    fn grok_benign_auth_token_is_not_auth_failure() {
        // grok always prints this benign secondary-worker line; with no clearer
        // auth phrasing it must NOT be classified as an auth failure.
        let (s, _) = classify(false, false, None,
            Some("non-zero exit (1): ERROR worker quit with fatal: Transport channel closed, when Auth(AuthorizationRequired)"));
        assert_eq!(s, ProbeStatus::Down, "bare Auth(AuthorizationRequired) is not an auth-failed signal");
    }

    #[test]
    fn unknown_failure_is_down() {
        let (s, _) = classify(false, false, None, Some("segfault"));
        assert_eq!(s, ProbeStatus::Down);
    }

    #[test]
    fn bare_numeric_codes_do_not_false_positive() {
        // F9: digits containing 429/401 in unrelated text must NOT classify as
        // rate_limited / auth_failed — only the textual phrasing does.
        for e in ["non-zero exit (1): wrote /tmp/429.log then crashed",
                  "panic at frame 0x401f20", "exit code 14012"] {
            let (s, _) = classify(false, false, None, Some(e));
            assert_eq!(s, ProbeStatus::Down, "bare-code substring must not misclassify: {e:?}");
        }
        // But the real textual errors still classify correctly.
        assert_eq!(classify(false, false, None, Some("429 Too Many Requests")).0, ProbeStatus::RateLimited);
        assert_eq!(classify(false, false, None, Some("401 Unauthorized")).0, ProbeStatus::AuthFailed);
    }

    #[test]
    fn real_gemini_quota_error_classified_and_reason_is_useful() {
        // The actual gemini-cli failure: a YOLO banner first, then the real
        // TerminalQuotaError deep in the output. Must be quota_exhausted, and the
        // reason must be the quota message (with reset time), NOT the banner.
        let err = "non-zero exit (1): YOLO mode is enabled. All tool calls will be automatically approved.\n\
                   Ripgrep is not available. Falling back to GrepTool.\n\
                   Error when talking to Gemini API Full report at: /var/folders/x/gemini-error.json TerminalQuotaError: out\n\
                   \"message\": \"You have exhausted your capacity on this model. Your quota will reset after 12h24m53s.\",\n\
                   reason: 'QUOTA_EXHAUSTED'";
        let (s, reason) = classify(false, false, None, Some(err));
        assert_eq!(s, ProbeStatus::QuotaExhausted);
        assert!(reason.contains("quota will reset after 12h24m53s"), "reason was: {reason:?}");
        assert!(!reason.to_lowercase().contains("yolo"), "banner leaked into reason: {reason:?}");
    }

    #[test]
    fn resolve_default_workers() {
        let w = resolve_workers(None).unwrap();
        assert_eq!(w, vec!["grok-cli", "gemini-cli", "codex-cli", "kiro-cli"]);
    }

    #[test]
    fn resolve_explicit_subset_and_reject_unknown() {
        assert_eq!(resolve_workers(Some("grok,codex")).unwrap(), vec!["grok-cli", "codex-cli"]);
        assert!(resolve_workers(Some("grok,nope")).is_err());
    }
}
