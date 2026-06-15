use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tmux_ai_parser::{Parser, events::Event};
use tmux_ai_parser::profile::CompiledProfile;
use tmux_ai_parser::learner::PatternStore;
use tmux_ai_io::TmuxSession;

pub const IDLE_THRESHOLD: Duration = Duration::from_millis(1500);

/// Tracks where a session is in the request/response cycle so a `poll` right
/// after a send can't report the PREVIOUS turn's terminal state (the
/// false-ready race). A send moves the session to `AwaitingStart`; only after
/// a `busy` poll is observed (`InProgress`) may a terminal state be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespPhase {
    /// No outstanding prompt — report raw state directly.
    Idle,
    /// A prompt was just sent; the worker hasn't been observed busy yet.
    AwaitingStart,
    /// The worker has produced output for the current prompt.
    InProgress,
}

/// Pure gate: given the raw poll state (`busy|ready|question|dead`) and the
/// current phase, decide whether to suppress a terminal state to `busy` and
/// what the next phase is. Suppression holds until the worker is observed
/// busy, which for any real multi-second LLM turn happens within a poll or two
/// — so it cannot deadlock in practice, and `wait` has a timeout backstop.
fn gate(raw: &str, phase: RespPhase) -> (bool, RespPhase) {
    match raw {
        "dead" => (false, RespPhase::Idle),
        "busy" => {
            let next = if phase == RespPhase::AwaitingStart { RespPhase::InProgress } else { phase };
            (false, next)
        }
        // terminal: ready | question
        _ => match phase {
            RespPhase::AwaitingStart => (true, RespPhase::AwaitingStart), // not started → suppress
            RespPhase::InProgress => (false, RespPhase::Idle),            // genuine completion
            RespPhase::Idle => (false, RespPhase::Idle),                  // no outstanding send
        },
    }
}

pub struct ManagedSession {
    pub tmux: TmuxSession,
    pub parser: Parser,
    resp_phase: RespPhase,
    last_activity: Instant,
}

impl ManagedSession {
    pub fn new(tmux: TmuxSession, parser: Parser) -> Self {
        Self { tmux, parser, resp_phase: RespPhase::Idle, last_activity: Instant::now() }
    }

    fn note_sent(&mut self) {
        self.resp_phase = RespPhase::AwaitingStart;
        self.tmux.note_input_sent();
        self.last_activity = Instant::now();
    }

    /// Time since the last send or poll touched this session (for reaping).
    pub fn idle_for(&self) -> Duration {
        self.last_activity.elapsed()
    }

    pub async fn send_async(&mut self, text: &str) -> Result<(), String> {
        self.tmux.send(text).await?;
        self.note_sent();
        Ok(())
    }

    pub async fn send_and_parse(&mut self, text: &str) -> Result<Vec<Event>, String> {
        self.note_sent();
        let events = self.tmux.send_and_parse(text, &mut self.parser).await?;
        // The sync path already blocked until the response settled, so the
        // outstanding-prompt gate is satisfied — a later poll must not stay
        // stuck in AwaitingStart for a turn that already completed.
        self.resp_phase = RespPhase::Idle;
        self.last_activity = Instant::now();
        Ok(events)
    }

    pub async fn send_keys(&mut self, keys: &[String]) -> Result<(), String> {
        self.tmux.send_raw(keys).await?;
        self.note_sent();
        Ok(())
    }

    pub async fn interrupt(&mut self) -> Result<(), String> {
        self.last_activity = Instant::now();
        self.tmux.interrupt().await
    }

    /// One non-blocking status check: (state, full-snapshot events).
    /// States: busy | ready | question | dead. (timeout comes from wait().)
    pub async fn poll(&mut self) -> Result<(String, Vec<Event>), String> {
        self.last_activity = Instant::now();
        if !tmux_ai_io::session::session_exists(self.tmux.name()).await.unwrap_or(false) {
            self.resp_phase = RespPhase::Idle;
            return Ok(("dead".into(), Vec::new()));
        }
        let idle = self.tmux.check_idle(IDLE_THRESHOLD).await?;
        let text = self.tmux.capture_now(200).await?;
        let events = self.parser.parse_snapshot(&text);
        let raw = if !idle {
            "busy"
        } else {
            // Scrollback keeps stale prompts AND stale questions from earlier
            // turns — whichever signal occurs LAST in the snapshot is current.
            let last_ready = events.iter().rposition(|e| matches!(e, Event::Ready));
            let last_question = events.iter().rposition(|e| matches!(e, Event::Question { .. }));
            match (last_ready, last_question) {
                (Some(r), Some(q)) if q > r => "question",
                (Some(_), _) => "ready",
                (None, Some(_)) => "question",
                (None, None) => "busy",
            }
        };
        let (suppress, next_phase) = gate(raw, self.resp_phase);
        self.resp_phase = next_phase;
        let state = if suppress { "busy" } else { raw };
        Ok((state.to_string(), events))
    }

    /// Poll until non-busy or timeout. Returns ("timeout", last events) on cap.
    pub async fn wait(&mut self, timeout: Duration) -> Result<(String, Vec<Event>), String> {
        let start = std::time::Instant::now();
        loop {
            let (state, events) = self.poll().await?;
            if state != "busy" {
                return Ok((state, events));
            }
            if start.elapsed() >= timeout {
                return Ok(("timeout".into(), events));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{gate, RespPhase};

    #[test]
    fn stale_ready_right_after_send_is_suppressed() {
        // Sent a prompt (AwaitingStart); the pane still shows the previous
        // turn's Ready. Must report busy, not the stale ready.
        let (suppress, next) = gate("ready", RespPhase::AwaitingStart);
        assert!(suppress, "stale ready after send must be suppressed to busy");
        assert_eq!(next, RespPhase::AwaitingStart, "stay awaiting until busy seen");
    }

    #[test]
    fn busy_advances_awaiting_to_in_progress() {
        let (suppress, next) = gate("busy", RespPhase::AwaitingStart);
        assert!(!suppress);
        assert_eq!(next, RespPhase::InProgress);
    }

    #[test]
    fn ready_after_observed_busy_is_genuine_completion() {
        let (suppress, next) = gate("ready", RespPhase::InProgress);
        assert!(!suppress, "ready after a busy cycle is the real answer");
        assert_eq!(next, RespPhase::Idle);
    }

    #[test]
    fn question_after_busy_passes_through() {
        let (suppress, next) = gate("question", RespPhase::InProgress);
        assert!(!suppress);
        assert_eq!(next, RespPhase::Idle);
    }

    #[test]
    fn no_outstanding_send_reports_directly() {
        // Idle phase = nothing pending; a `text`/`poll` with no prior send.
        assert_eq!(gate("ready", RespPhase::Idle), (false, RespPhase::Idle));
        assert_eq!(gate("question", RespPhase::Idle), (false, RespPhase::Idle));
    }

    #[test]
    fn dead_always_resets_and_passes() {
        for p in [RespPhase::Idle, RespPhase::AwaitingStart, RespPhase::InProgress] {
            assert_eq!(gate("dead", p), (false, RespPhase::Idle));
        }
    }
}

struct SessionEntry {
    profile: String,
    handle: Arc<Mutex<ManagedSession>>,
}

pub struct SessionManager {
    sessions: HashMap<String, SessionEntry>,
    profiles: Vec<CompiledProfile>,
    profiles_dir: PathBuf,
    store: Option<PatternStore>,
}

impl SessionManager {
    pub fn new(profiles_dir: &std::path::Path, db_path: Option<&std::path::Path>) -> Result<Self, String> {
        let mut profiles = Vec::new();
        if let Ok(entries) = std::fs::read_dir(profiles_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "toml").unwrap_or(false) {
                    match CompiledProfile::load(&path) {
                        Ok(p) => { eprintln!("[session_mgr] loaded profile: {}", p.name); profiles.push(p); }
                        Err(e) => eprintln!("[session_mgr] skip {}: {e}", path.display()),
                    }
                }
            }
        }
        let store = db_path.map(|p| PatternStore::open(p)).transpose()?;
        Ok(Self {
            sessions: HashMap::new(),
            profiles,
            profiles_dir: profiles_dir.to_path_buf(),
            store,
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.sessions.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<Mutex<ManagedSession>>> {
        self.sessions.get(name).map(|e| e.handle.clone())
    }

    pub fn remove(&mut self, name: &str) -> Option<Arc<Mutex<ManagedSession>>> {
        self.sessions.remove(name).map(|e| e.handle)
    }

    /// Resolve profile, wire the parser, insert. Called with an ALREADY
    /// spawned TmuxSession so the manager lock is never held across the
    /// 5s startup wait — concurrent creates and sends stay unblocked.
    pub async fn finish_create(
        &mut self,
        name: &str,
        mut tmux: TmuxSession,
        command: &str,
        profile_hint: Option<&str>,
    ) -> Result<(), String> {
        if self.sessions.contains_key(name) {
            let _ = tmux.kill().await;
            return Err(format!("session '{name}' already exists"));
        }

        // Any failure past this point must kill the freshly spawned tmux
        // session — otherwise it leaks as a zombie outside the manager's map.
        let compiled = match self.resolve_and_load(&mut tmux, command, profile_hint).await {
            Ok(c) => c,
            Err(e) => {
                let _ = tmux.kill().await;
                return Err(e);
            }
        };

        tmux.set_submit_keys(compiled.submit_keys.clone());
        tmux.set_modes(compiled.mode_indicator.clone(), compiled.mode_insert_key.clone());

        let profile_name = compiled.name.clone();
        let mut parser = Parser::new(compiled);
        if let Some(ref store) = self.store {
            let _ = parser.load_learned_patterns(store);
        }

        self.sessions.insert(name.to_string(), SessionEntry {
            profile: profile_name,
            handle: Arc::new(Mutex::new(ManagedSession::new(tmux, parser))),
        });
        Ok(())
    }

    /// Resolve the profile (explicit hint > auto-detect > command-name
    /// fallback) and load its compiled form from disk.
    /// `&mut self` (not `&self`): a shared borrow would capture the non-Sync
    /// PatternStore across the detect_profile await and un-Send the future.
    async fn resolve_and_load(
        &mut self,
        tmux: &mut TmuxSession,
        command: &str,
        profile_hint: Option<&str>,
    ) -> Result<CompiledProfile, String> {
        let profile = if let Some(hint) = profile_hint {
            self.profiles.iter().find(|p| p.name == hint || p.cli_command == hint)
                .map(|p| p.name.clone())
                .ok_or_else(|| format!("profile '{hint}' not found"))?
        } else if let Some(idx) = tmux.detect_profile(&self.profiles).await? {
            self.profiles[idx].name.clone()
        } else {
            let cmd_name = command.split_whitespace().next().unwrap_or("");
            self.profiles.iter()
                .find(|p| cmd_name.contains(&p.cli_command) || p.cli_command.contains(cmd_name))
                .map(|p| p.name.clone())
                .ok_or_else(|| format!("no matching profile for command '{cmd_name}'"))?
        };

        CompiledProfile::load(&self.find_profile_path(&profile)?)
            .map_err(|e| format!("load profile: {e}"))
    }

    fn find_profile_path(&self, name: &str) -> Result<PathBuf, String> {
        let candidates = [
            self.profiles_dir.join(format!("{name}.toml")),
            PathBuf::from(format!("profiles/{name}.toml")),
            dirs_next::home_dir().unwrap_or_default()
                .join(format!(".config/tmuxai/profiles/{name}.toml")),
        ];
        for c in &candidates {
            if c.exists() { return Ok(c.clone()); }
        }
        Err(format!("profile file not found for '{name}'"))
    }

    pub fn list_sessions(&self) -> Vec<(String, String)> {
        self.sessions.iter().map(|(k, v)| (k.clone(), v.profile.clone())).collect()
    }

    pub async fn kill_all(&mut self) {
        let names: Vec<String> = self.sessions.keys().cloned().collect();
        for name in names {
            if let Some(handle) = self.remove(&name) {
                let _ = handle.lock().await.tmux.kill().await;
            }
        }
    }

    /// Remove (but do not yet kill) sessions idle longer than `max_idle`,
    /// returning their handles so the caller can kill them AFTER dropping the
    /// manager lock — killing runs a `tmux` subprocess per session and must not
    /// block other requests behind the manager lock. Uses `try_lock` so a
    /// session held by an in-flight `wait`/`send` (active) is skipped, never
    /// blocked. Disabled when `max_idle` is zero.
    pub fn take_idle(&mut self, max_idle: Duration) -> Vec<(String, Arc<Mutex<ManagedSession>>)> {
        if max_idle.is_zero() {
            return Vec::new();
        }
        let stale: Vec<String> = self.sessions.iter()
            .filter_map(|(name, entry)| match entry.handle.try_lock() {
                Ok(guard) if guard.idle_for() > max_idle => Some(name.clone()),
                _ => None, // locked (active) or fresh → keep
            })
            .collect();
        stale.into_iter()
            .filter_map(|name| self.remove(&name).map(|h| (name, h)))
            .collect()
    }
}
