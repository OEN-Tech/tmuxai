use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use serde::{Deserialize, Serialize};
use serde_json;
use tmux_ai_parser::events::Event;
use tmux_ai_io::TmuxSession;
use tokio::sync::Mutex;

use crate::session_mgr::{ManagedSession, SessionManager};

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Command {
    CreateSession { create_session: CreateSessionArgs },
    Subscribe { subscribe: String },
    SendInput { send_input: SendInputArgs },
    SendAsync { send_async: SendInputArgs },
    SendKeys { send_keys: SendKeysArgs },
    GetState { get_state: String },
    ListSessions {
        #[allow(dead_code)] // serde discriminant tag; matched, not read
        list_sessions: bool,
    },
    KillSession { kill_session: String },
    Poll { poll: String },
    Wait { wait: WaitArgs },
    Interrupt { interrupt: String },
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionArgs {
    pub name: String,
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Explicit profile name. Skips auto-detection if provided.
    pub profile: Option<String>,
}

fn default_cwd() -> String { ".".into() }

#[derive(Debug, Deserialize)]
pub struct SendInputArgs {
    pub session: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct SendKeysArgs {
    pub session: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub session: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 { 120 }

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok { ok: bool, #[serde(skip_serializing_if = "Option::is_none")] session: Option<String> },
    Events { events: Vec<Event>, session: String },
    Poll { state: String, events: Vec<Event>, session: String },
    State { state: String, session: String },
    Sessions { sessions: Vec<SessionInfo> },
    Error { error: String },
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub profile: String,
}

/// Clone the per-session handle under a SHORT manager lock; all awaiting
/// happens against the session's own mutex so other sessions stay live.
async fn session_of(
    mgr: &Arc<Mutex<SessionManager>>,
    name: &str,
) -> Result<Arc<Mutex<ManagedSession>>, Response> {
    mgr.lock().await.get(name)
        .ok_or_else(|| Response::Error { error: format!("session '{name}' not found") })
}

async fn process_command(cmd: Command, mgr: &Arc<Mutex<SessionManager>>) -> Response {
    match cmd {
        Command::CreateSession { create_session: args } => {
            // TOCTOU window: a concurrent create with the same name can pass this
            // pre-check too; the loser's finish_create kills the shared tmux name and
            // the survivor self-heals via poll()->dead. Accepted for a single-operator
            // daemon with uniquely named sessions.
            {
                let m = mgr.lock().await;
                if m.contains(&args.name) {
                    return Response::Error { error: format!("session '{}' already exists", args.name) };
                }
            }
            // 5s startup wait runs with NO lock held — creates parallelize.
            let tmux = match TmuxSession::spawn(&args.name, &args.command, &args.cwd).await {
                Ok(t) => t,
                Err(e) => return Response::Error { error: e },
            };
            let handle = {
                let mut m = mgr.lock().await;
                match m.finish_create(&args.name, tmux, &args.command, args.profile.as_deref()).await {
                    Ok(()) => m.get(&args.name),
                    Err(e) => return Response::Error { error: e },
                }
            };
            // Block (manager lock released) until the CLI is actually READY before
            // acknowledging the spawn — a slow startup/auth (gemini's OAuth ~15-20s
            // exceeds the fixed startup wait) would otherwise let the first send
            // land in a not-yet-ready pane and be silently lost. Best-effort; capped.
            if let Some(h) = handle {
                let _ = h.lock().await.wait_ready(Duration::from_secs(30)).await;
            }
            Response::Ok { ok: true, session: Some(args.name) }
        }
        Command::SendInput { send_input: args } => {
            let sess = match session_of(mgr, &args.session).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.send_and_parse(&args.text).await;
            match result {
                Ok(events) => Response::Events { events, session: args.session },
                Err(e) => Response::Error { error: e },
            }
        }
        Command::SendAsync { send_async: args } => {
            let sess = match session_of(mgr, &args.session).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.send_async(&args.text).await;
            match result {
                Ok(()) => Response::Ok { ok: true, session: Some(args.session) },
                Err(e) => Response::Error { error: e },
            }
        }
        Command::SendKeys { send_keys: args } => {
            let sess = match session_of(mgr, &args.session).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.send_keys(&args.keys).await;
            match result {
                Ok(()) => Response::Ok { ok: true, session: Some(args.session) },
                Err(e) => Response::Error { error: e },
            }
        }
        Command::Poll { poll: name } => {
            let sess = match session_of(mgr, &name).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.poll().await;
            match result {
                Ok((state, events)) => {
                    if state == "dead" {
                        mgr.lock().await.remove(&name);
                    }
                    Response::Poll { state, events, session: name }
                }
                Err(e) => Response::Error { error: e },
            }
        }
        Command::Wait { wait: args } => {
            let sess = match session_of(mgr, &args.session).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.wait(Duration::from_secs(args.timeout_secs)).await;
            match result {
                Ok((state, events)) => {
                    if state == "dead" {
                        mgr.lock().await.remove(&args.session);
                    }
                    Response::Poll { state, events, session: args.session }
                }
                Err(e) => Response::Error { error: e },
            }
        }
        Command::Interrupt { interrupt: name } => {
            let sess = match session_of(mgr, &name).await { Ok(s) => s, Err(r) => return r };
            let result = sess.lock().await.interrupt().await;
            match result {
                Ok(()) => Response::Ok { ok: true, session: Some(name) },
                Err(e) => Response::Error { error: e },
            }
        }
        Command::GetState { get_state: name } => {
            let sess = match session_of(mgr, &name).await { Ok(s) => s, Err(r) => return r };
            let state = { sess.lock().await.parser.state() };
            Response::State { state: format!("{state:?}"), session: name }
        }
        Command::ListSessions { .. } => {
            let m = mgr.lock().await;
            let sessions = m.list_sessions().into_iter().map(|(name, profile)| SessionInfo {
                name, profile,
            }).collect();
            Response::Sessions { sessions }
        }
        Command::KillSession { kill_session: name } => {
            let handle = { mgr.lock().await.remove(&name) };
            match handle {
                None => Response::Error { error: format!("session '{name}' not found") },
                Some(h) => match h.lock().await.tmux.kill().await {
                    Ok(()) => Response::Ok { ok: true, session: Some(name) },
                    Err(e) => Response::Error { error: e },
                },
            }
        }
        Command::Subscribe { subscribe: _name } => {
            // TODO: implement pub/sub with mpsc channels
            Response::Ok { ok: true, session: None }
        }
    }
}

async fn handle_client(stream: UnixStream, mgr: Arc<Mutex<SessionManager>>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => process_command(cmd, &mgr).await,
            Err(e) => Response::Error { error: format!("parse: {e}") },
        };

        let mut json = serde_json::to_string(&response).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#));
        json.push('\n');
        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}

pub async fn run_server(socket_path: &Path, mgr: Arc<Mutex<SessionManager>>) -> Result<(), String> {
    // Refuse to clobber a live daemon's socket: concurrent cold-start clients
    // can race ensure_daemon; this probe makes losers exit instead of
    // unlinking the winner's socket file.
    if UnixStream::connect(socket_path).await.is_ok() {
        return Err(format!("another daemon already serves {}", socket_path.display()));
    }

    // Clean up stale socket
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;

    eprintln!("[daemon] listening on {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let mgr = mgr.clone();
                tokio::spawn(async move {
                    handle_client(stream, mgr).await;
                });
            }
            Err(e) => eprintln!("[daemon] accept error: {e}"),
        }
    }
}
