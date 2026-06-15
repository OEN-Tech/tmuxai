use crate::events::{TaskStatus, StatusBarInfo};
use crate::profile::CompiledProfile;

#[derive(Debug, Clone, PartialEq)]
pub enum LineClass {
    PromptEmpty,
    PromptInput(String),
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

pub fn classify_line(line: &str, profile: &CompiledProfile) -> LineClass {
    let trimmed = line.trim_end();

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
            return LineClass::PromptInput(m.as_str().to_string());
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
