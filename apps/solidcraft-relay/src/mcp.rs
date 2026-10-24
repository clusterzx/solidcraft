//! `/mcp`: the MCP server over Streamable HTTP (MCP 2025-06-18 §Transports), JSON responses only.
//!
//! - POST: one JSON-RPC message (or batch). A request gets `200` with its response; notifications
//!   and responses alone get `202`. `initialize` starts a session and says its id in
//!   `Mcp-Session-Id`, which every later request carries (missing: `400`, unknown or ended: `404`).
//! - GET: `405` (the relay starts no server-sent event stream).
//! - DELETE: ends the session.
//!
//! Every request shows the relay's token (`Authorization: Bearer …`), or gets `401`. Each session
//! has a thread of its own holding its [`Server`] and its view of the tab, so a long tool call
//! in one session doesn't hold up another.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solidcraft_mcp::{SUPPORTED_VERSIONS, Server, TabBackend};

use crate::http::{Request, Response};

/// Sessions at once; the one unused longest makes room for a new one.
pub const MAX_SESSIONS: usize = 64;
/// A session unused this long is ended.
pub const SESSION_IDLE: Duration = Duration::from_secs(24 * 3600);
/// The session threads' stack: commands of the relay's headless session run on them.
const STACK: usize = 32 << 20;

/// A message for a session's thread and where its answer goes.
type Job = (Value, Sender<Option<Value>>);

struct Session {
    jobs: Sender<Job>,
    last: Mutex<Instant>,
    /// Requests being answered now (a busy session is not ended to make room).
    busy: AtomicUsize,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Session>>>,
}

impl Sessions {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// A new session with its own thread, or why not.
    fn create(&self, relay: &crate::Relay, app_url: String) -> Result<(String, Arc<Session>), String> {
        let id = session_id()?;
        let (jobs, rx) = channel::<Job>();
        let (hub, mode, headless, waits) = (relay.hub.clone(), relay.config.headless, relay.headless.clone(), relay.config.waits);
        std::thread::Builder::new()
            .name("mcp-session".into())
            .stack_size(STACK)
            .spawn(move || {
                let backend = TabBackend::new(hub, mode, headless, &app_url).with_waits(waits);
                let mut server = Server::new(Box::new(backend));
                for (msg, reply) in rx {
                    let _ = reply.send(server.handle_value(msg));
                }
            })
            .map_err(|e| format!("no thread for a new session: {e}"))?;
        let session = Arc::new(Session { jobs, last: Mutex::new(Instant::now()), busy: AtomicUsize::new(0) });
        let mut map = self.lock();
        let now = Instant::now();
        let idle = |s: &Session| now.duration_since(*s.last.lock().unwrap_or_else(PoisonError::into_inner));
        map.retain(|_, s| s.busy.load(Ordering::Relaxed) > 0 || idle(s) < SESSION_IDLE);
        if map.len() >= MAX_SESSIONS {
            let oldest = map.iter().filter(|(_, s)| s.busy.load(Ordering::Relaxed) == 0).max_by_key(|(_, s)| idle(s)).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => return Err(format!("{MAX_SESSIONS} sessions are busy; try again when one is done")),
            }
        }
        map.insert(id.clone(), session.clone());
        Ok((id, session))
    }

    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.lock().get(id).cloned()
    }

    fn remove(&self, id: &str) -> bool {
        self.lock().remove(id).is_some()
    }
}

/// 32 hex digits from the operating system's random source.
fn session_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random session id: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Ask `session`'s thread to answer `msg`.
fn ask(session: &Session, msg: Value) -> Result<Option<Value>, String> {
    let (tx, rx) = channel();
    session.busy.fetch_add(1, Ordering::Relaxed);
    let sent = session.jobs.send((msg, tx));
    let answer = sent.ok().and_then(|()| rx.recv().ok());
    session.busy.fetch_sub(1, Ordering::Relaxed);
    if let Ok(mut last) = session.last.lock() {
        *last = Instant::now();
    }
    answer.ok_or_else(|| "the session ended while answering".to_string())
}

fn rpc_error(status: u16, code: i64, message: &str) -> Response {
    Response::json(status, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}}))
}

/// The bearer token in `Authorization`, compared in constant time.
pub fn authorized(req: &Request, token: &str) -> bool {
    let Some(value) = req.header("authorization") else { return false };
    let mut parts = value.splitn(2, ' ');
    let (scheme, given) = (parts.next().unwrap_or(""), parts.next().unwrap_or("").trim());
    scheme.eq_ignore_ascii_case("bearer") && solidcraft_engine::control_auth::tokens_match(given, token)
}

/// The web app's address as this request reached the relay: the proxy's host and scheme, and the
/// path before `/mcp`.
fn app_url(relay: &crate::Relay, req: &Request) -> String {
    if let Some(u) = &relay.config.public_url {
        return u.clone();
    }
    let first = |h: &str| req.header(h).and_then(|v| v.split(',').next()).map(str::trim).filter(|v| !v.is_empty());
    let clean = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(*c)).take(253).collect::<String>();
    let host = clean(first("x-forwarded-host").or_else(|| first("host")).unwrap_or("HOST"));
    // Through a proxy, https unless it says otherwise (saving designs needs it); straight to the
    // relay, http.
    let proxied = req.headers.iter().any(|(n, _)| n.starts_with("x-forwarded-") || n == "forwarded");
    let scheme = match first("x-forwarded-proto") {
        Some(p) if p.eq_ignore_ascii_case("http") => "http",
        Some(_) => "https",
        None if proxied => "https",
        None => "http",
    };
    let prefix = req.path.strip_suffix("mcp").unwrap_or("/");
    let prefix: String = prefix.chars().filter(|c| c.is_ascii_alphanumeric() || "-._~/".contains(*c)).take(256).collect();
    format!("{scheme}://{host}{prefix}")
}

pub fn handle(relay: &crate::Relay, req: &Request) -> Response {
    if !authorized(req, &relay.config.token) {
        return Response::json(401, &json!({"error": "unauthorized: send the relay's token as `Authorization: Bearer <token>`"}))
            .header("WWW-Authenticate", "Bearer realm=\"solidcraft-relay\"");
    }
    // A page in a browser may not drive the relay unless it is the relay's own site (or one
    // allowed by name): DNS rebinding.
    if let Some(origin) = req.header("origin")
        && !crate::bridge::origin_allowed(origin, req, &relay.config.allowed_origins)
    {
        return Response::json(403, &json!({"error": "this origin may not use the relay (SOLIDCRAFT_RELAY_ALLOWED_ORIGIN)"}));
    }
    match req.method.as_str() {
        "POST" => post(relay, req),
        "DELETE" => match req.header("mcp-session-id") {
            Some(id) if relay.sessions.remove(id) => Response::new(204),
            Some(_) => rpc_error(404, -32001, "session not found"),
            None => rpc_error(400, -32600, "missing Mcp-Session-Id"),
        },
        _ => Response::json(405, &json!({"error": "the relay answers POST (and DELETE to end a session); it opens no event stream"}))
            .header("Allow", "POST, DELETE"),
    }
}

fn post(relay: &crate::Relay, req: &Request) -> Response {
    if let Some(t) = req.header("content-type")
        && !t.to_ascii_lowercase().starts_with("application/json")
    {
        return rpc_error(415, -32600, "send JSON (Content-Type: application/json)");
    }
    let msg: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return rpc_error(400, -32700, &format!("parse error: {e}")),
    };
    let initialize = msg.get("method").and_then(Value::as_str) == Some("initialize") && msg.get("id").is_some();
    let (id, session) = if initialize {
        match relay.sessions.create(relay, app_url(relay, req)) {
            Ok(s) => s,
            Err(e) => return rpc_error(503, -32603, &e),
        }
    } else {
        let Some(id) = req.header("mcp-session-id") else {
            return rpc_error(400, -32600, "missing Mcp-Session-Id: start with initialize");
        };
        match relay.sessions.get(id) {
            Some(s) => (id.to_string(), s),
            None => return rpc_error(404, -32001, "session not found: it ended (or the relay restarted); start a new one with initialize"),
        }
    };
    if let Some(v) = req.header("mcp-protocol-version")
        && !SUPPORTED_VERSIONS.contains(&v)
    {
        return rpc_error(400, -32600, &format!("unsupported MCP-Protocol-Version {v}; supported: {}", SUPPORTED_VERSIONS.join(", ")));
    }
    let reply = match ask(&session, msg) {
        Ok(r) => r,
        Err(e) => {
            relay.sessions.remove(&id);
            return rpc_error(500, -32603, &e);
        }
    };
    let response = match reply {
        Some(r) => Response::json(200, &r),
        None => Response::new(202),
    };
    if initialize {
        eprintln!("solidcraft-relay: MCP session started ({} open)", relay.sessions.count());
        response.header("Mcp-Session-Id", id)
    } else {
        response
    }
}
