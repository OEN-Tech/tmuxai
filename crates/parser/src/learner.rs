use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use regex::Regex;
use std::path::Path;
use std::time::SystemTime;

// ── Pattern Store (SQLite) ──

pub struct PatternStore {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPattern {
    pub id: i64,
    pub profile_name: String,
    pub event_type: String,
    pub regex: String,
    pub confidence: f64,
    pub hit_count: u32,
    pub promoted: bool,
}

impl PatternStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open db: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS learned_patterns (
                id INTEGER PRIMARY KEY,
                profile_name TEXT NOT NULL,
                event_type TEXT NOT NULL,
                regex TEXT NOT NULL,
                confidence REAL NOT NULL,
                hit_count INTEGER DEFAULT 0,
                promoted INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                last_seen TEXT NOT NULL
            );"
        ).map_err(|e| format!("create table: {e}"))?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open memory db: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS learned_patterns (
                id INTEGER PRIMARY KEY,
                profile_name TEXT NOT NULL,
                event_type TEXT NOT NULL,
                regex TEXT NOT NULL,
                confidence REAL NOT NULL,
                hit_count INTEGER DEFAULT 0,
                promoted INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                last_seen TEXT NOT NULL
            );"
        ).map_err(|e| format!("create table: {e}"))?;
        Ok(Self { conn })
    }

    /// Record a classification from the LLM. Increments hit_count if same pattern exists.
    pub fn record(&self, profile: &str, event_type: &str, regex: &str, confidence: f64) -> Result<(), String> {
        let now = now_iso();
        let existing: Option<i64> = self.conn.query_row(
            "SELECT id FROM learned_patterns WHERE profile_name = ?1 AND regex = ?2",
            params![profile, regex],
            |row| row.get(0),
        ).ok();

        if let Some(id) = existing {
            self.conn.execute(
                "UPDATE learned_patterns SET hit_count = hit_count + 1, last_seen = ?1, confidence = MAX(confidence, ?2) WHERE id = ?3",
                params![now, confidence, id],
            ).map_err(|e| format!("update: {e}"))?;
        } else {
            self.conn.execute(
                "INSERT INTO learned_patterns (profile_name, event_type, regex, confidence, hit_count, promoted, created_at, last_seen) VALUES (?1, ?2, ?3, ?4, 1, 0, ?5, ?5)",
                params![profile, event_type, regex, confidence, now],
            ).map_err(|e| format!("insert: {e}"))?;
        }
        Ok(())
    }

    /// Promote patterns that have been seen >= threshold times. Returns newly promoted patterns.
    pub fn promote(&self, threshold: u32) -> Result<Vec<LearnedPattern>, String> {
        self.conn.execute(
            "UPDATE learned_patterns SET promoted = 1 WHERE hit_count >= ?1 AND promoted = 0",
            params![threshold],
        ).map_err(|e| format!("promote: {e}"))?;

        let mut stmt = self.conn.prepare(
            "SELECT id, profile_name, event_type, regex, confidence, hit_count, promoted FROM learned_patterns WHERE promoted = 1"
        ).map_err(|e| format!("query: {e}"))?;

        let patterns = stmt.query_map([], |row| {
            Ok(LearnedPattern {
                id: row.get(0)?,
                profile_name: row.get(1)?,
                event_type: row.get(2)?,
                regex: row.get(3)?,
                confidence: row.get(4)?,
                hit_count: row.get(5)?,
                promoted: row.get::<_, bool>(6)?,
            })
        }).map_err(|e| format!("map: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(patterns)
    }

    /// Get all promoted patterns for a profile (for loading into the fast path).
    pub fn get_promoted(&self, profile: &str) -> Result<Vec<LearnedPattern>, String> {
        let mut stmt = self.conn.prepare(
            "SELECT id, profile_name, event_type, regex, confidence, hit_count, promoted FROM learned_patterns WHERE profile_name = ?1 AND promoted = 1"
        ).map_err(|e| format!("query: {e}"))?;

        let patterns = stmt.query_map(params![profile], |row| {
            Ok(LearnedPattern {
                id: row.get(0)?,
                profile_name: row.get(1)?,
                event_type: row.get(2)?,
                regex: row.get(3)?,
                confidence: row.get(4)?,
                hit_count: row.get(5)?,
                promoted: row.get::<_, bool>(6)?,
            })
        }).map_err(|e| format!("map: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(patterns)
    }
}

fn now_iso() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

// ── LLM Fallback Classifier ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmClassification {
    pub event_type: String,
    pub regex: String,
    pub confidence: f64,
}

/// Build the prompt for the LLM to classify an unrecognized block.
pub fn build_classification_prompt(cli_name: &str, raw_blocks: &[String]) -> String {
    let blocks = raw_blocks.iter()
        .enumerate()
        .map(|(i, b)| format!("Block {}:\n```\n{b}\n```", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
r#"You are a terminal output classifier. Given blocks of text from a {cli_name} terminal session, classify each as one of these event types:

- assistant_text: The AI's response text
- tool_use: A tool being invoked
- tool_result: Output from a tool
- thinking: A thinking/processing indicator
- skill_load: A skill being loaded
- checklist: A task list with status markers
- status_bar: Model/cost/token information
- banner: Startup banner or welcome screen
- permission: A permission prompt
- error: An error message
- decoration: Visual decoration (borders, separators, icons)
- unknown: Cannot determine

For each block, suggest a regex pattern that would match similar lines.

{blocks}

Respond as a JSON array:
[{{"block": 1, "event_type": "...", "regex": "...", "confidence": 0.0-1.0}}]"#
    )
}

/// Parse the LLM response into classifications.
pub fn parse_classification_response(response: &str) -> Vec<LlmClassification> {
    // Try to find JSON array in the response
    let start = response.find('[');
    let end = response.rfind(']');
    if let (Some(s), Some(e)) = (start, end) {
        if let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&response[s..=e]) {
            return items.iter().filter_map(|v| {
                Some(LlmClassification {
                    event_type: v.get("event_type")?.as_str()?.to_string(),
                    regex: v.get("regex")?.as_str()?.to_string(),
                    confidence: v.get("confidence")?.as_f64().unwrap_or(0.5),
                })
            }).collect();
        }
    }
    Vec::new()
}

// ── Compiled Learned Patterns (fast path) ──

pub struct CompiledLearnedPattern {
    pub event_type: String,
    pub regex: Regex,
}

/// Compile promoted patterns into regex for the fast path.
pub fn compile_learned(patterns: &[LearnedPattern]) -> Vec<CompiledLearnedPattern> {
    patterns.iter().filter_map(|p| {
        Regex::new(&p.regex).ok().map(|re| CompiledLearnedPattern {
            event_type: p.event_type.clone(),
            regex: re,
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_store_record_and_promote() {
        let store = PatternStore::open_in_memory().unwrap();

        // Record same pattern 3 times
        store.record("claude-code", "banner", r"^╭───", 0.9).unwrap();
        store.record("claude-code", "banner", r"^╭───", 0.9).unwrap();
        store.record("claude-code", "banner", r"^╭───", 0.95).unwrap();

        // Not yet promoted (threshold = 3)
        let promoted = store.promote(4).unwrap();
        assert!(promoted.iter().all(|p| p.regex != r"^╭───" || p.hit_count < 4));

        // Now promote at threshold 3
        let promoted = store.promote(3).unwrap();
        let banner = promoted.iter().find(|p| p.regex == r"^╭───");
        assert!(banner.is_some());
        assert_eq!(banner.unwrap().hit_count, 3);
        assert!(banner.unwrap().promoted);
    }

    #[test]
    fn test_parse_llm_response() {
        let response = r#"Here are the classifications:
[{"block": 1, "event_type": "banner", "regex": "^╭───", "confidence": 0.95}]"#;
        let results = parse_classification_response(response);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "banner");
        assert_eq!(results[0].confidence, 0.95);
    }

    #[test]
    fn test_build_prompt() {
        let prompt = build_classification_prompt("claude-code", &["╭─── Claude Code".into()]);
        assert!(prompt.contains("claude-code"));
        assert!(prompt.contains("╭─── Claude Code"));
        assert!(prompt.contains("event_type"));
    }
}
