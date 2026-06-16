use crate::events::{TaskStatus, StatusBarInfo};
use crate::profile::CompiledProfile;

#[derive(Debug, Clone, PartialEq)]
pub enum LineClass {
    PromptEmpty,
    /// An echoed user prompt (`❯ …`). `indent` is the leading-whitespace width of
    /// the `❯`, used by prefix-less CLIs to tell a WRAPPED prompt continuation
    /// (indented deeper, under the prompt text) from the answer (at the `❯` indent).
    PromptInput { text: String, indent: usize },
    Separator,
    AssistantText(String),
    ToolUse { tool: String, args: String },
    ToolResult(String),
    Truncated(u32),
    SkillLoad(String),
    Thinking { label: String, detail: Option<String> },
    /// Explicit end-of-response marker (profile `response_end`); flushes the
    /// pending answer and returns the session to idle.
    ResponseEnd,
    TaskItem { status: TaskStatus, text: String },
    StatusBar(StatusBarInfo),
    ErrorLine(String),
    Unrecognized(String),
    Empty,
}

/// Strip grok's right-margin scrollbar thumb: a trailing run of █ separated
/// from the text by whitespace. Requires the leading whitespace so a contiguous
/// block glyph attached to content (e.g. a progress bar `[████]`) is left intact.
/// Returns the text with the trailing scrollbar (and the spaces before it)
/// removed; a pure-scrollbar row collapses to "".
fn strip_scrollbar(line: &str) -> &str {
    let t = line.trim_end();
    if !t.ends_with('█') {
        return t;
    }
    // A row that is ONLY whitespace + █ (any width) is a pure scrollbar row.
    if t.trim_end_matches('█').trim().is_empty() {
        return "";
    }
    // Otherwise strip only a LONE trailing █ — the 1-column scrollbar thumb —
    // that floats after whitespace. A MULTI-█ run is real content (a bar chart
    // or progress bar like "A:  ████████"), and a █ glued directly to a word is
    // content too; both are left intact. This is gap-independent, so it also
    // strips the scrollbar off a long line where the text nearly reaches the
    // margin (only one space before the █).
    let body = &t[..t.len() - '█'.len_utf8()];
    if body.ends_with('█') {
        return t; // multi-█ run = content
    }
    if body.ends_with([' ', '\t']) {
        body.trim_end()
    } else {
        t // █ glued to a word = content
    }
}

pub fn classify_line(line: &str, profile: &CompiledProfile) -> LineClass {
    // Prefix-less TUIs (grok) draw a scrollbar thumb — a run of █ block chars —
    // at the far right margin, overlaying both blank rows AND the right edge of
    // text rows ("…paths.<spaces>█"). Strip that trailing column so a pure
    // scrollbar row becomes Empty and a text row isn't polluted with a trailing
    // █. Gated on text_after_thinking (grok) and guarded to require whitespace
    // before the █ run, so a contiguous block glyph in real content (e.g. a
    // progress bar "[████]") is never touched.
    let trimmed = if profile.text_after_thinking {
        strip_scrollbar(line)
    } else {
        line.trim_end()
    };

    if trimmed.is_empty() {
        return LineClass::Empty;
    }

    // Prompt (empty)
    if profile.prompt_empty.is_match(trimmed) {
        return LineClass::PromptEmpty;
    }

    // Prompt with input
    if let Some(caps) = profile.prompt_input.captures(trimmed) {
        if let Some(m) = caps.get(1) {
            // Leading-whitespace width of the line = the `❯` indent (the prompt
            // marker is preceded only by spaces).
            let indent = trimmed.len() - trimmed.trim_start().len();
            return LineClass::PromptInput { text: m.as_str().to_string(), indent };
        }
    }

    // Separator
    if profile.separator.is_match(trimmed) {
        return LineClass::Separator;
    }

    // Skill load (before tool_use since it's more specific)
    if let Some(ref re) = profile.skill_load {
        if let Some(caps) = re.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                return LineClass::SkillLoad(m.as_str().to_string());
            }
        }
    }

    // Tool use
    if let Some(ref re) = profile.tool_use {
        if let Some(caps) = re.captures(trimmed) {
            let tool = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let args = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            return LineClass::ToolUse { tool, args };
        }
    }

    // Thinking
    if let Some(ref re) = profile.thinking {
        if let Some(caps) = re.captures(trimmed) {
            let label = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let detail = caps.get(2).map(|m| m.as_str().to_string());
            return LineClass::Thinking { label, detail };
        }
    }

    // End-of-response marker (prefix-less CLIs like grok)
    if let Some(ref re) = profile.response_end {
        if re.is_match(trimmed) {
            return LineClass::ResponseEnd;
        }
    }

    // Tool result
    if let Some(ref re) = profile.tool_result {
        if let Some(caps) = re.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                return LineClass::ToolResult(m.as_str().to_string());
            }
        }
    }

    // Truncated
    if let Some(ref re) = profile.truncated {
        if let Some(caps) = re.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                if let Ok(n) = m.as_str().parse::<u32>() {
                    return LineClass::Truncated(n);
                }
            }
        }
    }

    // Task items
    if !profile.task_done.is_empty() && trimmed.contains(&profile.task_done) {
        let text = trimmed.replace(&profile.task_done, "").trim().to_string();
        return LineClass::TaskItem { status: TaskStatus::Done, text };
    }
    if !profile.task_active.is_empty() && trimmed.contains(&profile.task_active) {
        let text = trimmed.replace(&profile.task_active, "").trim().to_string();
        return LineClass::TaskItem { status: TaskStatus::Active, text };
    }
    if !profile.task_pending.is_empty() && trimmed.contains(&profile.task_pending) {
        let text = trimmed.replace(&profile.task_pending, "").trim().to_string();
        return LineClass::TaskItem { status: TaskStatus::Pending, text };
    }

    // Error
    for re in &profile.error_patterns {
        if re.is_match(trimmed) {
            return LineClass::ErrorLine(trimmed.to_string());
        }
    }

    // Status bar
    if let Some(ref re) = profile.status_bar {
        if let Some(caps) = re.captures(trimmed) {
            return LineClass::StatusBar(parse_status_bar(&caps, trimmed, profile));
        }
    }

    // Assistant text (starts with prefix)
    if !profile.assistant_prefix.is_empty() && trimmed.starts_with(&profile.assistant_prefix) {
        let text = trimmed[profile.assistant_prefix.len()..].trim_start().to_string();
        return LineClass::AssistantText(text);
    }

    // Ignorable UI chrome (e.g. Gemini's "? for shortcuts" / skills bar). Checked
    // last, after every real marker, so it only ever swallows leftover chrome.
    // Treated as Empty so it never becomes Unrecognized->continuation text.
    for re in &profile.chrome_patterns {
        if re.is_match(trimmed) {
            return LineClass::Empty;
        }
    }

    LineClass::Unrecognized(trimmed.to_string())
}

fn parse_status_bar(caps: &regex::Captures, full_line: &str, profile: &CompiledProfile) -> StatusBarInfo {
    let model = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
    let tokens_in = caps.get(2).and_then(|m| parse_token_count(m.as_str()));
    let tokens_out = caps.get(3).and_then(|m| parse_token_count(m.as_str()));
    let cost = caps.get(4).and_then(|m| m.as_str().parse::<f64>().ok());
    let context_pct = caps.get(6).and_then(|m| m.as_str().parse::<f32>().ok());
    let peak_status = profile.peak_indicator.as_ref().and_then(|re| {
        re.find(full_line).map(|m| m.as_str().to_string())
    });
    let memory_pct = profile.memory.as_ref().and_then(|re| {
        re.captures(full_line).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<u32>().ok())
    });
    StatusBarInfo { model, cost, tokens_in, tokens_out, context_pct, peak_status, memory_pct }
}

fn parse_token_count(s: &str) -> Option<u64> {
    if s.ends_with('k') {
        s[..s.len()-1].parse::<f64>().ok().map(|v| (v * 1000.0) as u64)
    } else {
        s.parse::<u64>().ok()
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::strip_scrollbar;

    #[test]
    fn strips_trailing_thumb_after_whitespace() {
        assert_eq!(strip_scrollbar("   exercised paths.            █"), "   exercised paths.");
        assert_eq!(strip_scrollbar("nits.     █"), "nits.");
    }

    #[test]
    fn pure_scrollbar_row_collapses_to_empty() {
        assert_eq!(strip_scrollbar("                  █"), "");
        assert_eq!(strip_scrollbar("   ██  "), "");
    }

    #[test]
    fn multi_block_run_is_content_not_scrollbar() {
        // Review finding G4: a bar chart / progress bar must NOT be mistaken for
        // the scrollbar, regardless of the whitespace gap before it.
        assert_eq!(strip_scrollbar("[████]"), "[████]");
        assert_eq!(strip_scrollbar("progress: ████"), "progress: ████");
        assert_eq!(strip_scrollbar("A:  ████████"), "A:  ████████");
        assert_eq!(strip_scrollbar("x  ██"), "x  ██");
    }

    #[test]
    fn strips_lone_thumb_even_with_small_gap() {
        // Review finding G5: on a long line the text nearly reaches the margin, so
        // there is only ONE space before the scrollbar █ — it must still be stripped.
        assert_eq!(strip_scrollbar("a very long line that reaches the right edge █"),
                   "a very long line that reaches the right edge");
    }

    #[test]
    fn block_glued_to_word_is_kept() {
        assert_eq!(strip_scrollbar("done█"), "done█");
    }

    #[test]
    fn plain_line_unchanged() {
        assert_eq!(strip_scrollbar("just normal text"), "just normal text");
        assert_eq!(strip_scrollbar("trailing spaces   "), "trailing spaces");
    }
}
