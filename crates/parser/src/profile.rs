use regex::Regex;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct ProfileToml {
    pub meta: MetaSection,
    pub prompt: PromptSection,
    pub separator: SeparatorSection,
    pub markers: MarkersSection,
    pub tasks: TasksSection,
    pub status_bar: StatusBarSection,
    pub question: QuestionSection,
    pub error: ErrorSection,
    #[serde(default)]
    pub modes: Option<ModesSection>,
    #[serde(default)]
    pub chrome: Option<ChromeSection>,
    #[serde(default)]
    pub exec: Option<ExecSection>,
}

#[derive(Debug, Deserialize)]
pub struct ChromeSection {
    /// Regexes for post-response / persistent UI chrome lines (e.g. Gemini's
    /// "? for shortcuts", the auto-accept/skills bar, the status row). Matching
    /// lines are treated as ignorable (classified as Empty) so they never get
    /// appended to the assistant text as continuation.
    pub patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExecSection {
    /// Headless invocation template. `{prompt}` is replaced with the prompt as a
    /// single argv element (the command is NOT run through a shell). When
    /// `use_stdin` is true, `{prompt}` is dropped and the prompt is piped on stdin.
    pub command: String,
    #[serde(default = "default_exec_output")]
    pub output: String,
    /// jq-style path into the JSON answer (required iff output = "json").
    #[serde(default)]
    pub answer_path: String,
    #[serde(default)]
    pub use_stdin: bool,
}

fn default_exec_output() -> String {
    "text".to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecOutput {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct CompiledExec {
    pub command: String,
    pub output: ExecOutput,
    pub answer_path: String,
    pub use_stdin: bool,
}

pub fn parse_exec_output(s: &str) -> Result<ExecOutput, String> {
    match s {
        "text" => Ok(ExecOutput::Text),
        "json" => Ok(ExecOutput::Json),
        other => Err(format!("exec.output must be 'text' or 'json', got '{other}'")),
    }
}

/// Cross-field validation for an `[exec]` section. JSON output is meaningless
/// without a path to extract the answer, so reject an empty `answer_path` at
/// profile-load time instead of failing later at runtime.
pub fn validate_exec(output: &ExecOutput, answer_path: &str) -> Result<(), String> {
    if *output == ExecOutput::Json && answer_path.is_empty() {
        return Err("exec.answer_path is required when output = \"json\"".into());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct MetaSection {
    pub name: String,
    pub cli_command: String,
    pub banner: String,
    pub version_capture: String,
    #[serde(default)]
    pub launch_command: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModesSection {
    pub indicator: String,
    pub insert_key: String,
}

#[derive(Debug, Deserialize)]
pub struct PromptSection {
    pub pattern: String,
    pub input: String,
    #[serde(default)]
    pub submit_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SeparatorSection {
    pub pattern: String,
}

#[derive(Debug, Deserialize)]
pub struct MarkersSection {
    pub assistant_prefix: String,
    pub tool_use: String,
    pub tool_result: String,
    pub truncated: String,
    pub thinking: String,
    pub skill_load: String,
    /// Optional explicit end-of-response marker (e.g. grok's "Turn completed
    /// in 2.5s."). When matched, the current response flushes and the session
    /// goes idle. Lets prefix-less CLIs bound their answer.
    #[serde(default)]
    pub response_end: String,
    /// For CLIs whose answer text has NO leading glyph (grok prints bare text
    /// after a "◆ Thought for …" line). When true, the first plain line after
    /// a thinking marker starts the response. Opt-in — prefixed CLIs leave it
    /// false and are unaffected.
    #[serde(default)]
    pub text_after_thinking: bool,
}

#[derive(Debug, Deserialize)]
pub struct TasksSection {
    pub done: String,
    pub active: String,
    pub pending: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusBarSection {
    pub pattern: String,
    pub peak_indicator: String,
    pub memory: String,
}

#[derive(Debug, Deserialize)]
pub struct QuestionSection {
    pub choice_pattern: String,
}

#[derive(Debug, Deserialize)]
pub struct ErrorSection {
    pub patterns: Vec<String>,
}

/// Compiled profile with pre-built Regex objects
#[derive(Debug)]
pub struct CompiledProfile {
    pub name: String,
    pub cli_command: String,
    pub banner: Regex,
    pub version_capture: Regex,
    pub prompt_empty: Regex,
    pub prompt_input: Regex,
    /// Keys to send after text to submit input. Default: ["Enter"].
    /// Gemini needs ["Escape", "Enter"] due to vim-like modal input.
    pub submit_keys: Vec<String>,
    /// Full default launch command for `tmuxai spawn` (e.g. "gemini -m gemini-3.5-flash …").
    pub launch_command: Option<String>,
    /// Regex extracting the editor mode from the pane (e.g. `\[(INSERT|NORMAL)\]`).
    pub mode_indicator: Option<Regex>,
    /// Key that enters INSERT mode when the pane is not in it.
    pub mode_insert_key: Option<String>,
    pub separator: Regex,
    pub assistant_prefix: String,
    pub tool_use: Option<Regex>,
    pub tool_result: Option<Regex>,
    pub truncated: Option<Regex>,
    pub thinking: Option<Regex>,
    pub skill_load: Option<Regex>,
    pub response_end: Option<Regex>,
    pub text_after_thinking: bool,
    pub task_done: String,
    pub task_active: String,
    pub task_pending: String,
    pub status_bar: Option<Regex>,
    pub peak_indicator: Option<Regex>,
    pub memory: Option<Regex>,
    pub choice_pattern: Option<Regex>,
    pub error_patterns: Vec<Regex>,
    /// Ignorable UI-chrome line patterns (see `[chrome]` in the profile).
    pub chrome_patterns: Vec<Regex>,
    /// Optional headless one-shot invocation (`tmuxai run` / `fanout --mode exec`).
    pub exec: Option<CompiledExec>,
}

fn compile_optional(s: &str) -> Option<Regex> {
    if s.is_empty() { None } else { Regex::new(s).ok() }
}

impl CompiledProfile {
    pub fn from_toml(toml: &ProfileToml) -> Result<Self, String> {
        Ok(Self {
            name: toml.meta.name.clone(),
            cli_command: toml.meta.cli_command.clone(),
            banner: Regex::new(&toml.meta.banner).map_err(|e| format!("banner: {e}"))?,
            version_capture: Regex::new(&toml.meta.version_capture).map_err(|e| format!("version: {e}"))?,
            prompt_empty: Regex::new(&toml.prompt.pattern).map_err(|e| format!("prompt: {e}"))?,
            prompt_input: Regex::new(&toml.prompt.input).map_err(|e| format!("prompt input: {e}"))?,
            submit_keys: if toml.prompt.submit_keys.is_empty() {
                vec!["Enter".to_string()]
            } else {
                toml.prompt.submit_keys.clone()
            },
            launch_command: toml.meta.launch_command.clone(),
            mode_indicator: toml.modes.as_ref()
                .map(|m| Regex::new(&m.indicator).map_err(|e| format!("modes.indicator: {e}")))
                .transpose()?,
            mode_insert_key: toml.modes.as_ref().map(|m| m.insert_key.clone()),
            separator: Regex::new(&toml.separator.pattern).map_err(|e| format!("separator: {e}"))?,
            assistant_prefix: toml.markers.assistant_prefix.clone(),
            tool_use: compile_optional(&toml.markers.tool_use),
            tool_result: compile_optional(&toml.markers.tool_result),
            truncated: compile_optional(&toml.markers.truncated),
            thinking: compile_optional(&toml.markers.thinking),
            skill_load: compile_optional(&toml.markers.skill_load),
            response_end: compile_optional(&toml.markers.response_end),
            text_after_thinking: toml.markers.text_after_thinking,
            task_done: toml.tasks.done.clone(),
            task_active: toml.tasks.active.clone(),
            task_pending: toml.tasks.pending.clone(),
            status_bar: compile_optional(&toml.status_bar.pattern),
            peak_indicator: compile_optional(&toml.status_bar.peak_indicator),
            memory: compile_optional(&toml.status_bar.memory),
            choice_pattern: compile_optional(&toml.question.choice_pattern),
            error_patterns: toml.error.patterns.iter().filter_map(|p| Regex::new(p).ok()).collect(),
            chrome_patterns: toml.chrome.as_ref()
                .map(|c| c.patterns.iter().filter_map(|p| Regex::new(p).ok()).collect())
                .unwrap_or_default(),
            exec: match toml.exec.as_ref() {
                None => None,
                Some(e) => {
                    let output = parse_exec_output(&e.output)?;
                    validate_exec(&output, &e.answer_path)?;
                    Some(CompiledExec {
                        command: e.command.clone(),
                        output,
                        answer_path: e.answer_path.clone(),
                        use_stdin: e.use_stdin,
                    })
                }
            },
        })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let toml: ProfileToml = toml::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))?;
        Self::from_toml(&toml)
    }

    pub fn load_by_name(name: &str) -> Result<Self, String> {
        // Search in standard locations
        let candidates = [
            format!("profiles/{name}.toml"),
            format!("../profiles/{name}.toml"),
            format!("../../profiles/{name}.toml"),
        ];
        for c in &candidates {
            let p = Path::new(c);
            if p.exists() {
                return Self::load(p);
            }
        }
        Err(format!("profile '{name}' not found"))
    }

    pub fn detect_from_text(text: &str, profiles: &[CompiledProfile]) -> Option<usize> {
        let first_lines: String = text.lines().take(5).collect::<Vec<_>>().join("\n");
        profiles.iter().position(|p| p.banner.is_match(&first_lines))
    }
}

#[cfg(test)]
mod exec_tests {
    use super::*;

    #[test]
    fn parse_exec_output_accepts_text_and_json() {
        assert_eq!(parse_exec_output("text").unwrap(), ExecOutput::Text);
        assert_eq!(parse_exec_output("json").unwrap(), ExecOutput::Json);
    }

    #[test]
    fn parse_exec_output_rejects_unknown() {
        assert!(parse_exec_output("xml").is_err());
    }

    #[test]
    fn validate_exec_rejects_json_without_answer_path() {
        let err = validate_exec(&ExecOutput::Json, "").unwrap_err();
        assert!(err.contains("answer_path"), "error must mention answer_path, got: {err}");
    }

    #[test]
    fn validate_exec_accepts_json_with_answer_path() {
        assert!(validate_exec(&ExecOutput::Json, ".result").is_ok());
    }

    #[test]
    fn validate_exec_accepts_text_without_answer_path() {
        // answer_path is irrelevant for text output, so empty is fine.
        assert!(validate_exec(&ExecOutput::Text, "").is_ok());
    }
}
