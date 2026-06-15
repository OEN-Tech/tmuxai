pub mod events;
pub mod profile;
pub mod classifier;
pub mod fsm;
pub mod status_bar;
pub mod question;
pub mod learner;

use events::{Event, State};
use profile::CompiledProfile;
use classifier::classify_line;
use fsm::Fsm;
use learner::{CompiledLearnedPattern, compile_learned, PatternStore};

pub struct Parser {
    profile: CompiledProfile,
    fsm: Fsm,
    prev_line_count: usize,
    learned: Vec<CompiledLearnedPattern>,
    unrecognized_buffer: Vec<String>,
}

impl Parser {
    pub fn new(profile: CompiledProfile) -> Self {
        let fsm = Fsm::with_text_after_thinking(profile.text_after_thinking);
        Self { profile, fsm, prev_line_count: 0, learned: Vec::new(), unrecognized_buffer: Vec::new() }
    }

    /// Load promoted patterns from a PatternStore into the fast path.
    pub fn load_learned_patterns(&mut self, store: &PatternStore) -> Result<(), String> {
        let patterns = store.get_promoted(&self.profile.name)?;
        self.learned = compile_learned(&patterns);
        Ok(())
    }

    /// Parse a full capture-pane snapshot.
    /// Ink TUI apps rewrite the entire pane on each render, so we parse
    /// all lines every time and use the FSM to produce events.
    pub fn parse_snapshot(&mut self, text: &str) -> Vec<Event> {
        // Reset FSM for fresh parse — Ink rewrites the whole pane
        self.fsm = Fsm::with_text_after_thinking(self.profile.text_after_thinking);
        self.unrecognized_buffer.clear();

        let mut events = Vec::new();
        for line in text.lines() {
            let class = classify_line(line, &self.profile);
            // If unrecognized, check learned patterns first
            if let classifier::LineClass::Unrecognized(ref raw) = class {
                match self.try_learned(raw) {
                    Some(evt) => { events.push(evt); continue; }
                    None if self.is_learned_skip(raw) => { continue; } // banner/decoration
                    None => { self.unrecognized_buffer.push(raw.clone()); }
                }
            }
            events.extend(self.fsm.feed(class));
        }
        events
    }

    /// Parse all lines (ignore previous state). Useful for fixtures.
    pub fn parse_all(&mut self, text: &str) -> Vec<Event> {
        self.prev_line_count = 0;
        self.parse_snapshot(text)
    }

    /// Drain the unrecognized buffer (for sending to LLM fallback).
    pub fn drain_unrecognized(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unrecognized_buffer)
    }

    pub fn state(&self) -> State {
        self.fsm.state()
    }

    pub fn profile(&self) -> &CompiledProfile {
        &self.profile
    }

    fn try_learned(&self, line: &str) -> Option<Event> {
        for lp in &self.learned {
            if lp.regex.is_match(line) {
                return match lp.event_type.as_str() {
                    "banner" | "decoration" => None, // handled by is_learned_skip
                    "assistant_text" => Some(Event::AssistantText { text: line.to_string(), is_complete: false }),
                    "error" => Some(Event::Error { message: line.to_string() }),
                    _ => None,
                };
            }
        }
        None
    }

    fn is_learned_skip(&self, line: &str) -> bool {
        for lp in &self.learned {
            if lp.regex.is_match(line) {
                return matches!(lp.event_type.as_str(), "banner" | "decoration");
            }
        }
        false
    }
}
