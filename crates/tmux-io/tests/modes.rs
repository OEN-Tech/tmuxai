use tmux_ai_io::extract_mode;

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../parser/tests/fixtures/{name}"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("load fixture {}: {e}", p.display()))
}

#[test]
fn gemini_fixture_modes_detected() {
    let re = regex::Regex::new(r"\[(INSERT|NORMAL)\]").unwrap();
    assert_eq!(extract_mode(&fixture("gemini-0.46-normal.txt"), &re).as_deref(), Some("NORMAL"));
    assert_eq!(extract_mode(&fixture("gemini-0.45-insert.txt"), &re).as_deref(), Some("INSERT"));
}

#[test]
fn last_mode_indicator_wins() {
    let re = regex::Regex::new(r"\[(INSERT|NORMAL)\]").unwrap();
    assert_eq!(extract_mode("a [INSERT] b\nc [NORMAL] d", &re).as_deref(), Some("NORMAL"));
    assert_eq!(extract_mode("no modes here", &re), None);
}
