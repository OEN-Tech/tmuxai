use crate::events::Choice;
use regex::Regex;
use std::sync::LazyLock;

static CHOICE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*-?\s*\*?\*?([A-Z])\)?\*?\*?\s+(.+)").unwrap()
});

/// Split assistant text into the question body and any detected choices.
/// Returns (text_without_choices, choices_vec).
pub fn extract_choices(text: &str) -> (String, Vec<Choice>) {
    let mut choices = Vec::new();
    let mut body_lines = Vec::new();

    for line in text.lines() {
        if let Some(caps) = CHOICE_RE.captures(line) {
            let key = caps.get(1).unwrap().as_str().to_string();
            let choice_text = caps.get(2).unwrap().as_str().trim().to_string();
            choices.push(Choice { key, text: choice_text });
        } else {
            body_lines.push(line);
        }
    }

    (body_lines.join("\n").trim().to_string(), choices)
}
