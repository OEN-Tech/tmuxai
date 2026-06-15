use tmux_ai_parser::{Parser, events::*};
use tmux_ai_parser::profile::{CompiledProfile, ExecOutput};
use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn profile(name: &str) -> CompiledProfile {
    CompiledProfile::load(&workspace_root().join(format!("profiles/{name}.toml")))
        .unwrap_or_else(|e| panic!("load profile {name}: {e}"))
}

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("load fixture {}: {e}", p.display()))
}

#[test]
fn kiro_classic_prompt_matches_with_and_without_lambda() {
    let p = profile("kiro-cli");
    assert!(p.prompt_empty.is_match("5% >"), "kiro-cli 2.5.1 classic prompt has no λ");
    assert!(p.prompt_empty.is_match("3% λ >"), "legacy λ prompt must still match");
    assert!(p.prompt_input.is_match("4% > Reply with exactly: KIRO_CLASSIC_OK"));
}

#[test]
fn kiro_classic_fixture_parses_response_and_ready() {
    let mut parser = Parser::new(profile("kiro-cli"));
    let events = parser.parse_all(&fixture("kiro-classic-2.5.1.txt"));
    assert!(
        events.iter().any(|e| matches!(e, Event::AssistantText { text, .. } if text.contains("KIRO_CLASSIC_OK"))),
        "expected assistant text KIRO_CLASSIC_OK, got: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Ready)),
        "expected Ready (empty `N% >` prompt), got: {events:#?}"
    );
}

#[test]
fn claude_multiparagraph_response_not_truncated() {
    // Regression: a `⏺` response whose text spans multiple paragraphs separated
    // by a BLANK line must not be truncated. `tmuxai text` uses the LAST
    // AssistantText, so the second paragraph being split into a separate (later)
    // event would silently drop the first paragraph; if it became an
    // UnrecognizedBlock the second paragraph would vanish entirely.
    let mut parser = Parser::new(profile("claude-code"));
    let events = parser.parse_all(&fixture("claude-multipara.txt"));

    // The LAST AssistantText must contain BOTH paragraphs.
    let last = events.iter().rev().find_map(|e| {
        if let Event::AssistantText { text, .. } = e { Some(text.clone()) } else { None }
    });
    let last = last.unwrap_or_else(|| panic!("expected at least one AssistantText, got: {events:#?}"));
    assert!(
        last.contains("First paragraph") && last.contains("Second paragraph continues"),
        "multi-paragraph response truncated; last AssistantText = {last:?}; all events: {events:#?}"
    );

    // And the second paragraph must never have leaked out as an UnrecognizedBlock.
    assert!(
        !events.iter().any(|e| matches!(e, Event::UnrecognizedBlock { raw } if raw.contains("Second paragraph"))),
        "second paragraph leaked as UnrecognizedBlock: {events:#?}"
    );

    assert_eq!(parser.state(), State::Idle);
}

#[test]
fn gemini_post_response_chrome_excluded_from_text() {
    // Guard against re-introducing the original bug: Gemini's post-response UI
    // chrome ("? for shortcuts", the auto-accept/skills bar, the [NORMAL] status
    // row) must NOT be appended to the assistant text. The LAST AssistantText
    // must be exactly T1_OK with no chrome.
    let mut parser = Parser::new(profile("gemini-cli"));
    let events = parser.parse_all(&fixture("gemini-0.46-normal.txt"));

    let last = events.iter().rev().find_map(|e| {
        if let Event::AssistantText { text, .. } = e { Some(text.clone()) } else { None }
    });
    let last = last.unwrap_or_else(|| panic!("expected an AssistantText, got: {events:#?}"));
    assert_eq!(last.trim(), "T1_OK", "last AssistantText polluted with chrome: {last:?}");
    assert!(!last.contains("shortcuts"), "chrome '? for shortcuts' leaked: {last:?}");
    assert!(!last.contains("workspace"), "chrome 'workspace' header leaked: {last:?}");
    assert!(!last.contains("auto-accept"), "chrome 'auto-accept edits' bar leaked: {last:?}");
}

#[test]
fn profiles_expose_launch_command_and_modes() {
    let kiro = profile("kiro-cli");
    assert_eq!(
        kiro.launch_command.as_deref(),
        Some("kiro-cli chat --classic --trust-all-tools --model claude-opus-4.6")
    );
    assert!(kiro.mode_indicator.is_none(), "kiro has no vim modes");
    assert!(kiro.mode_insert_key.is_none(), "kiro has no insert key");

    let gem = profile("gemini-cli");
    assert_eq!(
        gem.launch_command.as_deref(),
        Some("gemini -m gemini-3-flash --approval-mode yolo --skip-trust")
    );
    let ind = gem.mode_indicator.as_ref().expect("gemini mode indicator");
    assert!(ind.is_match(" [NORMAL]   ~/Code/tmux-ai-parser"));
    assert!(ind.is_match(" [INSERT]   ~/Code"));
    assert_eq!(gem.mode_insert_key.as_deref(), Some("i"));

    assert_eq!(
        profile("claude-code").launch_command.as_deref(),
        Some("claude --dangerously-skip-permissions")
    );
    let cdx_lc = profile("codex-cli").launch_command.expect("codex launch_command");
    assert!(
        cdx_lc.contains("codex-fleet") && cdx_lc.contains("-m gpt-5.5"),
        "codex launches via the codex-fleet wrapper pinning gpt-5.5: {cdx_lc}"
    );
}

#[test]
fn gemini_yolo_ready_fixture_emits_ready_and_clean_text() {
    // Regression guard: the yolo-mode idle line " *   Press 'Esc' for NORMAL mode."
    // must match `prompt_empty` (^\s+\*\s+Press) and emit Event::Ready.
    // The assistant text must contain READY_PROBE_OK and must NOT contain chrome words.
    let mut parser = Parser::new(profile("gemini-cli"));
    let events = parser.parse_all(&fixture("gemini-yolo-ready.txt"));

    assert!(
        events.iter().any(|e| matches!(e, Event::Ready)),
        "expected Event::Ready from yolo `*`-prefix idle line, got: {events:#?}"
    );

    let last = events.iter().rev().find_map(|e| {
        if let Event::AssistantText { text, .. } = e { Some(text.clone()) } else { None }
    });
    let last = last.unwrap_or_else(|| panic!("expected an AssistantText, got: {events:#?}"));
    assert!(
        last.contains("READY_PROBE_OK"),
        "expected READY_PROBE_OK in last AssistantText, got: {last:?}"
    );
    assert!(!last.contains("shortcuts"), "chrome '? for shortcuts' leaked: {last:?}");
    assert!(!last.contains("workspace"), "chrome 'workspace' header leaked: {last:?}");
    assert!(!last.contains("model"), "chrome 'model' column leaked: {last:?}");
}

#[test]
fn gemini_input_regex_matches_echo_not_idle_lines() {
    // Regression guard: tightened `input = '^\s*> (\S.*)'` must match echoed
    // user prompts but NOT the NORMAL/INSERT idle lines.
    let gem = profile("gemini-cli");

    // Should match the echoed user prompt (first non-space after "> " is a real char)
    let cap = gem.prompt_input.captures("> Reply with exactly: TURN_2_OK");
    assert!(
        cap.is_some(),
        "input regex must match echoed prompt `> Reply with exactly: TURN_2_OK`"
    );
    assert_eq!(
        cap.unwrap().get(1).map(|m| m.as_str()),
        Some("Reply with exactly: TURN_2_OK"),
        "capture group must be the prompt text without leading `> `"
    );

    // Must NOT match NORMAL idle line (multiple spaces after "> ")
    assert!(
        !gem.prompt_input.is_match(">   Press 'i' for INSERT mode."),
        "input regex must NOT match NORMAL idle line `>   Press 'i' for INSERT mode.`"
    );

    // Must NOT match yolo/INSERT idle line (star prefix, no ">")
    assert!(
        !gem.prompt_input.is_match(" *   Press 'Esc' for NORMAL mode."),
        "input regex must NOT match INSERT idle line ` *   Press 'Esc' for NORMAL mode.`"
    );
}


// ---- Codex CLI 0.137 (gpt-5.5 xhigh) ----

#[test]
fn codex_launch_command_pins_gpt55_xhigh_yolo() {
    // codex launches via the codex-fleet wrapper (pre-trusts the canonical cwd so
    // the interactive trust gate never hangs a fleet worker — see bin/codex-fleet).
    let p = profile("codex-cli");
    let lc = p.launch_command.as_deref().expect("codex has a launch_command");
    assert!(lc.contains("codex-fleet"), "codex launches via codex-fleet: {lc}");
    assert!(lc.contains("--dangerously-bypass-approvals-and-sandbox"), "yolo mode: {lc}");
    assert!(lc.contains("-m gpt-5.5"), "pins gpt-5.5: {lc}");
    assert!(lc.contains("model_reasoning_effort=\"xhigh\""), "pins xhigh: {lc}");
    assert!(lc.contains("service_tier=\"fast\""), "pins fast tier: {lc}");
}

#[test]
fn codex_assistant_bullet_is_text_not_task() {
    // The `•` assistant prefix collides with a `•` task-active marker. Codex
    // answers must parse as AssistantText, never as a Checklist/TaskItem.
    let mut parser = Parser::new(profile("codex-cli"));
    let events = parser.parse_all(&fixture("codex-0.137-session.txt"));

    let last_text = events.iter().rev().find_map(|e| match e {
        Event::AssistantText { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        last_text.as_deref(),
        Some("CODEX_TUI_OK"),
        "codex `• CODEX_TUI_OK` must be the last AssistantText, got events: {events:#?}"
    );

    // The answer text must NOT have leaked into a Checklist event.
    let bad_checklist = events.iter().any(|e| matches!(
        e, Event::Checklist { tasks } if tasks.iter().any(|t| t.text.contains("CODEX_TUI_OK"))
    ));
    assert!(!bad_checklist, "codex answer must not be parsed as a task item");
}

#[test]
fn codex_idle_prompt_with_placeholder_emits_ready() {
    // Codex 0.137 idle prompt carries a rotating placeholder hint
    // (`› Summarize recent commits`); prompt_empty must still match it so the
    // orchestrator can reach `ready`.
    let p = profile("codex-cli");
    assert!(p.prompt_empty.is_match("› Summarize recent commits"), "placeholder idle prompt must match prompt_empty");
    assert!(p.prompt_empty.is_match("›"), "bare prompt must still match");

    let mut parser = Parser::new(profile("codex-cli"));
    let events = parser.parse_all(&fixture("codex-0.137-session.txt"));
    assert!(
        events.iter().any(|e| matches!(e, Event::Ready)),
        "expected Ready from the idle `›` prompt, got: {events:#?}"
    );
}

#[test]
fn codex_mcp_warning_chrome_excluded_from_text() {
    // `⚠ MCP …` startup noise must not pollute assistant text.
    let mut parser = Parser::new(profile("codex-cli"));
    let events = parser.parse_all(&fixture("codex-0.137-session.txt"));
    let leaked = events.iter().any(|e| matches!(
        e, Event::AssistantText { text, .. } if text.contains("MCP") || text.contains("Tip:")
    ));
    assert!(!leaked, "codex startup chrome leaked into AssistantText: {events:#?}");

    // Box-drawing frames must be swallowed as chrome, not surface as UnrecognizedBlock.
    let p = profile("codex-cli");
    assert!(p.chrome_patterns.iter().any(|re| re.is_match("╭──────────────╮")));
    assert!(p.chrome_patterns.iter().any(|re| re.is_match("╰──────────────╯")));
}

// ---- Grok Build CLI (grok-build) ----

#[test]
fn grok_launch_command_and_prefixless_flags() {
    let p = profile("grok-cli");
    assert_eq!(
        p.launch_command.as_deref(),
        Some("grok -m grok-composer-2.5-fast --no-alt-screen --always-approve --no-memory --effort high")
    );
    assert!(p.text_after_thinking, "grok answers are prefix-less; flag must be on");
    assert!(p.response_end.is_some(), "grok needs a response_end marker");
    assert!(p.response_end.as_ref().unwrap().is_match("  Turn completed in 2.5s."));
}

#[test]
fn grok_prompt_matches_idle_box_not_echoed_input() {
    let p = profile("grok-cli");
    // Idle input box (ready)
    assert!(p.prompt_empty.is_match("  │ ❯                          │"), "idle box must be Ready");
    // Echoed user turn is PromptInput, NOT the empty box
    assert!(!p.prompt_empty.is_match("  ❯ Now reply with exactly: GROK_TWO_OK   9:44 PM"));
    let cap = p.prompt_input.captures("  ❯ Now reply with exactly: GROK_TWO_OK   9:44 PM");
    assert!(cap.is_some(), "echoed prompt must be PromptInput");
}

#[test]
fn grok_prefixless_answer_extracted_between_thinking_and_turn_completed() {
    let mut parser = Parser::new(profile("grok-cli"));
    let events = parser.parse_all(&fixture("grok-session.txt"));
    let last = events.iter().rev().find_map(|e| match e {
        Event::AssistantText { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert!(
        last.as_deref().map(|t| t.contains("GROK_TWO_OK")).unwrap_or(false),
        "grok answer GROK_TWO_OK must be extracted as AssistantText, got: {events:#?}"
    );
    // The thinking line's own text must NOT leak into the answer.
    assert!(
        !last.as_deref().unwrap_or("").contains("Thought for"),
        "thinking line must not pollute the answer"
    );
    assert!(events.iter().any(|e| matches!(e, Event::Ready)), "idle ❯ box must emit Ready");
}

#[test]
fn grok_optin_does_not_affect_prefixed_profiles() {
    // The other four must NOT have the prefix-less behavior (regression guard).
    for name in ["claude-code", "codex-cli", "kiro-cli", "gemini-cli"] {
        let p = profile(name);
        assert!(!p.text_after_thinking, "{name} must keep text_after_thinking = false");
        assert!(p.response_end.is_none(), "{name} must have no response_end marker");
    }
}

#[test]
fn grok_exec_section_parses() {
    let p = profile("grok-cli");
    let exec = p.exec.as_ref().expect("grok-cli must have an [exec] section");
    assert!(exec.command.contains("-p {prompt}"), "command was: {}", exec.command);
    assert_eq!(exec.output, ExecOutput::Json);
    assert!(!exec.answer_path.is_empty(), "json output needs a non-empty answer_path");
}

#[test]
fn claude_code_has_no_exec_section() {
    assert!(profile("claude-code").exec.is_none(), "claude-code has no headless exec mode");
}
