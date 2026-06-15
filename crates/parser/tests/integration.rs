use tmux_ai_parser::{Parser, events::*};
use tmux_ai_parser::profile::CompiledProfile;
use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn load_profile() -> CompiledProfile {
    CompiledProfile::load(&workspace_root().join("profiles/claude-code.toml")).expect("load profile")
}

fn load_fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("load fixture {}: {e}", p.display()))
}

#[test]
fn test_detect_claude_code_banner() {
    let profile = load_profile();
    let text = load_fixture("simple-response.txt");
    let profiles = vec![profile];
    let idx = CompiledProfile::detect_from_text(&text, &profiles);
    assert_eq!(idx, Some(0));
}

#[test]
fn test_simple_response() {
    let profile = load_profile();
    let mut parser = Parser::new(profile);
    let text = load_fixture("simple-response.txt");
    let events = parser.parse_all(&text);

    // Should contain AssistantText with "Hello world"
    let has_hello = events.iter().any(|e| matches!(e, Event::AssistantText { text, .. } if text == "Hello world"));
    assert!(has_hello, "Expected AssistantText 'Hello world', got: {events:#?}");

    // Should end with Ready
    let has_ready = events.iter().any(|e| matches!(e, Event::Ready));
    assert!(has_ready, "Expected Ready event");

    // Final state should be Idle
    assert_eq!(parser.state(), State::Idle);
}

#[test]
fn test_brainstorm_has_question() {
    let profile = load_profile();
    let mut parser = Parser::new(profile);
    let text = load_fixture("brainstorm-turn1.txt");
    let events = parser.parse_all(&text);

    // Should have a SkillLoaded event
    let has_skill = events.iter().any(|e| matches!(e, Event::SkillLoaded { name } if name.contains("brainstorming")));
    assert!(has_skill, "Expected SkillLoaded for brainstorming, got: {events:#?}");

    // Should have a Question with choices A/B/C/D
    let question = events.iter().find(|e| matches!(e, Event::Question { .. }));
    assert!(question.is_some(), "Expected Question event, got: {events:#?}");
    if let Some(Event::Question { choices, .. }) = question {
        assert!(choices.len() >= 3, "Expected at least 3 choices, got {}", choices.len());
    }

    // Should have a Checklist
    let has_checklist = events.iter().any(|e| matches!(e, Event::Checklist { .. }));
    assert!(has_checklist, "Expected Checklist event");

    // Should end idle
    assert_eq!(parser.state(), State::Idle);
}

#[test]
fn test_brainstorm_checklist_tasks() {
    let profile = load_profile();
    let mut parser = Parser::new(profile);
    let text = load_fixture("brainstorm-turn1.txt");
    let events = parser.parse_all(&text);

    let checklist = events.iter().find_map(|e| {
        if let Event::Checklist { tasks } = e { Some(tasks) } else { None }
    });
    assert!(checklist.is_some());
    let tasks = checklist.unwrap();

    let done_count = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let active_count = tasks.iter().filter(|t| t.status == TaskStatus::Active).count();
    let pending_count = tasks.iter().filter(|t| t.status == TaskStatus::Pending).count();

    assert!(done_count >= 1, "Expected at least 1 done task");
    assert!(active_count >= 1, "Expected at least 1 active task");
    assert!(pending_count >= 1, "Expected at least 1 pending task");
}

#[test]
fn test_status_bar_parsed() {
    let profile = load_profile();
    let mut parser = Parser::new(profile);
    let text = load_fixture("simple-response.txt");
    let events = parser.parse_all(&text);

    let status = events.iter().find_map(|e| {
        if let Event::StatusBar(info) = e { Some(info) } else { None }
    });
    assert!(status.is_some(), "Expected StatusBar event, got: {events:#?}");
    let info = status.unwrap();
    assert!(info.cost.is_some(), "Expected cost");
    assert!(info.memory_pct.is_some(), "Expected memory %");
}

#[test]
fn test_learned_patterns_suppress_banner() {
    use tmux_ai_parser::learner::PatternStore;

    let profile = load_profile();
    let mut parser = Parser::new(profile);

    // First parse — simple fixture may have some unrecognized lines (permission mode, etc.)
    let text = load_fixture("simple-response.txt");
    let _events = parser.parse_all(&text);
    // This is fine — we just want to verify the self-updating loop below

    // Simulate: LLM classified the welcome banner box as "banner"
    let store = PatternStore::open_in_memory().unwrap();
    store.record("claude-code", "banner", r"^╭───", 0.95).unwrap();
    store.record("claude-code", "banner", r"^╭───", 0.95).unwrap();
    store.record("claude-code", "banner", r"^╭───", 0.95).unwrap();
    store.record("claude-code", "decoration", r"^╰───", 0.90).unwrap();
    store.record("claude-code", "decoration", r"^╰───", 0.90).unwrap();
    store.record("claude-code", "decoration", r"^╰───", 0.90).unwrap();

    // Promote (threshold 3)
    let promoted = store.promote(3).unwrap();
    assert!(promoted.len() >= 2, "Expected at least 2 promoted patterns");

    // Load into parser
    let profile2 = load_profile();
    let mut parser2 = Parser::new(profile2);
    parser2.load_learned_patterns(&store).unwrap();

    // Parse text with banner box — should now be silently skipped
    let banner_text = "╭─── Claude Code v2.1.92 ───────\n╰───────────────────────────────\n❯ \n";
    let events2 = parser2.parse_all(banner_text);
    let unrecognized2 = events2.iter().filter(|e| matches!(e, Event::UnrecognizedBlock { .. })).count();
    assert_eq!(unrecognized2, 0, "Banner should be suppressed by learned patterns, got: {events2:#?}");

    // Ready event should still be there
    assert!(events2.iter().any(|e| matches!(e, Event::Ready)));
}
