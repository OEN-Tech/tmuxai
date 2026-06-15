use crate::classifier::LineClass;
use crate::events::*;

pub struct Fsm {
    state: State,
    pending_text: Vec<String>,
    pending_tasks: Vec<Task>,
    /// Profile opt-in: the first plain line after a Thinking marker starts the
    /// response (for prefix-less CLIs like grok). Off for prefixed CLIs.
    text_after_thinking: bool,
}

impl Fsm {
    pub fn new() -> Self {
        Self::with_text_after_thinking(false)
    }

    pub fn with_text_after_thinking(text_after_thinking: bool) -> Self {
        Self { state: State::Idle, pending_text: Vec::new(), pending_tasks: Vec::new(), text_after_thinking }
    }

    pub fn state(&self) -> State { self.state }

    pub fn feed(&mut self, class: LineClass) -> Vec<Event> {
        let mut events = Vec::new();

        match class {
            LineClass::PromptEmpty => {
                self.flush_text(&mut events);
                self.flush_tasks(&mut events);
                self.transition(State::Idle, &mut events);
                events.push(Event::Ready);
            }
            LineClass::PromptInput(_) => {
                // A prompt line (live input box or an echoed prompt in scrollback)
                // is a turn boundary: the previous response is done. Flush it and
                // leave Responding so the new turn's text starts a fresh block and
                // the echoed prompt itself isn't appended as continuation.
                self.flush_text(&mut events);
                self.flush_tasks(&mut events);
                if self.state == State::Responding {
                    self.transition(State::Idle, &mut events);
                }
            }
            LineClass::Thinking { label, detail } => {
                self.flush_text(&mut events);
                self.transition(State::Thinking, &mut events);
                let elapsed_secs = detail.as_deref().and_then(parse_elapsed);
                events.push(Event::Thinking { label, elapsed_secs });
            }
            LineClass::SkillLoad(name) => {
                events.push(Event::SkillLoaded { name });
            }
            LineClass::ToolUse { tool, args } => {
                self.flush_text(&mut events);
                self.transition(State::ToolUse, &mut events);
                events.push(Event::ToolUse { tool, args });
            }
            LineClass::ToolResult(content) => {
                self.transition(State::ToolResult, &mut events);
                events.push(Event::ToolResult { content, truncated_lines: None });
            }
            LineClass::Truncated(n) => {
                // Amend the last ToolResult if possible
                if let Some(Event::ToolResult { truncated_lines, .. }) = events.last_mut() {
                    *truncated_lines = Some(n);
                }
            }
            LineClass::AssistantText(text) => {
                self.transition(State::Responding, &mut events);
                self.pending_text.push(text);
            }
            LineClass::TaskItem { status, text } => {
                if self.state != State::Checklist {
                    self.flush_text(&mut events);
                    self.transition(State::Checklist, &mut events);
                }
                self.pending_tasks.push(Task { status, text });
            }
            LineClass::ErrorLine(msg) => {
                self.flush_text(&mut events);
                self.transition(State::Error, &mut events);
                events.push(Event::Error { message: msg });
            }
            LineClass::StatusBar(info) => {
                events.push(Event::StatusBar(info));
            }
            LineClass::Separator => {
                // A separator bar is a hard block boundary — flush everything.
                self.flush_text(&mut events);
                self.flush_tasks(&mut events);
            }
            LineClass::ResponseEnd => {
                // Explicit end-of-response (grok's "Turn completed in …"): flush
                // the accumulated answer and return to idle.
                self.flush_text(&mut events);
                self.flush_tasks(&mut events);
                self.transition(State::Idle, &mut events);
            }
            LineClass::Empty => {
                // A bare blank line WITHIN a Responding block is a paragraph
                // break, not a boundary: keep accumulating so multi-paragraph
                // responses (Claude/Kiro `⏺ ... <blank> ... more`) stay joined
                // in a single AssistantText. Flushing here truncated them.
                if self.state == State::Responding {
                    self.pending_text.push(String::new());
                } else {
                    self.flush_text(&mut events);
                    self.flush_tasks(&mut events);
                }
            }
            LineClass::Unrecognized(raw) => {
                // If we're in Responding state, treat as continuation text.
                if self.state == State::Responding {
                    self.pending_text.push(raw);
                } else if self.text_after_thinking && self.state == State::Thinking {
                    // Prefix-less CLI (grok): the first plain line after a
                    // "◆ Thought for …" line IS the start of the answer.
                    self.transition(State::Responding, &mut events);
                    self.pending_text.push(raw);
                } else {
                    events.push(Event::UnrecognizedBlock { raw });
                }
            }
        }

        events
    }

    fn transition(&mut self, new: State, events: &mut Vec<Event>) {
        if self.state != new {
            events.push(Event::StateChange { from: self.state, to: new });
            self.state = new;
        }
    }

    fn flush_text(&mut self, events: &mut Vec<Event>) {
        if self.pending_text.is_empty() { return; }
        let full = self.pending_text.join("\n");
        self.pending_text.clear();
        let (text, choices) = crate::question::extract_choices(&full);
        if choices.is_empty() {
            // Don't emit a hollow AssistantText for a flush that contained only
            // blank lines / ignored chrome (e.g. after the real response already
            // flushed at a separator and only trailing chrome remained).
            if !text.is_empty() {
                events.push(Event::AssistantText { text, is_complete: true });
            }
        } else {
            events.push(Event::Question { text, choices });
        }
        // NOTE: do NOT force Idle here. The live-proof fix did, which truncated
        // multi-paragraph responses at the first blank line (para 1 flushed +
        // forced Idle, so para 2 became an UnrecognizedBlock). Post-response UI
        // chrome is instead kept out of the text by classifying it as ignorable
        // (see profile `[chrome]` patterns) so it never reaches the
        // Unrecognized->continuation rule.
    }

    fn flush_tasks(&mut self, events: &mut Vec<Event>) {
        if self.pending_tasks.is_empty() { return; }
        let tasks = std::mem::take(&mut self.pending_tasks);
        events.push(Event::Checklist { tasks });
    }
}

fn parse_elapsed(s: &str) -> Option<f64> {
    // Parse strings like "4s", "2m 34s", "1m 47s"
    let s = s.trim();
    let mut total = 0.0;
    for part in s.split_whitespace() {
        if let Some(rest) = part.strip_suffix('s') {
            if let Ok(v) = rest.parse::<f64>() { total += v; }
        } else if let Some(rest) = part.strip_suffix('m') {
            if let Ok(v) = rest.parse::<f64>() { total += v * 60.0; }
        }
    }
    if total > 0.0 { Some(total) } else { None }
}
