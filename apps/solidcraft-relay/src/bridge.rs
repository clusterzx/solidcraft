//! The browser tab's side: `/bridge`, a WebSocket the SolidCraft web app opens to the relay
//! (docs/web-mcp.md). A tab can't listen, so it connects out; the relay sends it control
//! requests (`{id, method, params}`) and it answers each (`{id, ok, result | error}`), as the
//! desktop control channel does. Its first message shows the relay's token. One tab at a time:
//! the newest wins, and the one before is closed with a reason (code 4000) and doesn't come back
//! by itself.

use std::collections::HashMap;
use std::io::{BufReader, ErrorKind};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solidcraft_mcp::TabLink;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use crate::http::{Request, Response};

/// Longest request sent to the tab (as the control channel's lines).
pub const MAX_REQUEST: usize = solidcraft_engine::MAX_LINE;
/// Longest message accepted from the tab: a reply may carry a screenshot (as `Remote`'s replies).
pub const MAX_REPLY: usize = 64 << 20;
/// Requests waiting in the tab at once (the tab queues as many, `MAX_PENDING` in its api.rs).
pub const MAX_IN_TAB: usize = 256;
/// Bridge connections at once, signed in or not.
pub const MAX_BRIDGES: usize = 8;
/// How long a new connection has to show the token.
pub const AUTH_WAIT: Duration = Duration::from_secs(10);
/// How often the relay says it is there, and how long it waits to hear from the tab.
pub const PING_EVERY: Duration = Duration::from_secs(20);
pub const SILENCE: Duration = Duration::from_secs(60);
/// Messages that are not JSON before the connection is closed.
const MAX_BAD_MESSAGES: usize = 8;

/// Close codes the tab acts on (docs/web-mcp.md): it reconnects after any other.
pub const CLOSE_REPLACED: u16 = 4000;
pub const CLOSE_UNAUTHORIZED: u16 = 4001;

/// Something to send to the tab, from any thread.
enum Outgoing {
    Text(String),
    Close(u16, String),
}

/// A request sent to the tab and not answered yet.
struct InFlight {
    reply: Sender<Value>,
    method: String,
    command: Option<String>,
    since: Instant,
}

struct Tab {
    number: u64,
    out: Sender<Outgoing>,
    in_flight: HashMap<u64, InFlight>,
}

/// The connected tab, shared by every MCP session (it is their [`TabLink`]).
pub struct Hub {
    tab: Mutex<Option<Tab>>,
    next_id: AtomicU64,
    numbers: AtomicU64,
    /// Bridge connections now (for [`MAX_BRIDGES`]).
    pub(crate) bridges: AtomicUsize,
}

impl Default for Hub {
    fn default() -> Self {
        Self { tab: Mutex::new(None), next_id: AtomicU64::new(1), numbers: AtomicU64::new(1), bridges: AtomicUsize::new(0) }
    }
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, Option<Tab>> {
        self.tab.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A tab signed in: it becomes the one, and the one before is told why it is closed. Its
    /// requests are lost with it (their callers hear the tab went away).
    fn register(&self) -> (u64, Receiver<Outgoing>) {
        let number = self.numbers.fetch_add(1, Ordering::Relaxed);
        let (out, rx) = channel();
        if let Some(old) = self.lock().replace(Tab { number, out, in_flight: HashMap::new() }) {
            let _ = old.out.send(Outgoing::Close(CLOSE_REPLACED, "replaced: another SolidCraft tab connected to the relay".into()));
            eprintln!("solidcraft-relay: tab {} replaced by tab {number}", old.number);
        }
        (number, rx)
    }

    /// Tab `number` went away (unless another replaced it already).
    fn unregister(&self, number: u64) {
        let mut tab = self.lock();
        if tab.as_ref().is_some_and(|t| t.number == number) {
            *tab = None;
        }
    }

    /// A reply from tab `number`.
    fn deliver(&self, number: u64, reply: Value) {
        let Some(id) = reply.get("id").and_then(Value::as_u64) else { return };
        let mut tab = self.lock();
        let Some(t) = tab.as_mut().filter(|t| t.number == number) else { return };
        if let Some(f) = t.in_flight.remove(&id) {
            let mut reply = reply;
            if let Some(o) = reply.as_object_mut() {
                o.remove("id");
            }
            let _ = f.reply.send(reply);
        }
    }
}

impl TabLink for Hub {
    fn send(&self, method: &str, params: Value) -> Result<Receiver<Value>, String> {
        let command = params.get("command").and_then(Value::as_str).filter(|c| c.len() <= 128).map(str::to_string);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        if line.len() > MAX_REQUEST {
            return Err(format!("the request is too large for the tab ({} bytes, at most {MAX_REQUEST})", line.len()));
        }
        let mut tab = self.lock();
        let Some(t) = tab.as_mut() else { return Err("no tab".into()) };
        if t.in_flight.len() >= MAX_IN_TAB {
            return Err(format!("{MAX_IN_TAB} requests already wait in the tab; send this one when they are done"));
        }
        let (reply, rx) = channel();
        t.out.send(Outgoing::Text(line)).map_err(|_| "no tab".to_string())?;
        t.in_flight.insert(id, InFlight { reply, method: method.chars().take(64).collect(), command, since: Instant::now() });
        Ok(rx)
    }

    fn connection(&self) -> Option<u64> {
        self.lock().as_ref().map(|t| t.number)
    }

    fn running(&self) -> Value {
        let tab = self.lock();
        let mut rows: Vec<(u64, Value)> = tab
            .iter()
            .flat_map(|t| t.in_flight.iter())
            .map(|(id, f)| {
                let mut r = json!({"id": id, "method": f.method, "running_s": (f.since.elapsed().as_secs_f64() * 10.0).round() / 10.0});
                if let Some(c) = &f.command {
                    r["command"] = json!(c);
                }
                (*id, r)
            })
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        json!({"running": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>()})
    }
}

/// Whether a page at `origin` may connect: one of the allowed origins, or the relay's own (the
/// host the request came to, as the proxy in front passes it on).
pub fn origin_allowed(origin: &str, req: &Request, allowed: &[String]) -> bool {
    let origin = crate::config::normal_origin(origin);
    if !allowed.is_empty() {
        return allowed.contains(&origin);
    }
    let Some(authority) = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://")) else { return false };
    let host = req.header("x-forwarded-host").and_then(|h| h.split(',').next()).or_else(|| req.header("host")).unwrap_or("");
    let host = host.trim().to_ascii_lowercase();
    // A default port may be left out on either side.
    let bare = |a: &str| a.strip_suffix(":443").or_else(|| a.strip_suffix(":80")).unwrap_or(a).to_string();
    !host.is_empty() && bare(authority) == bare(&host)
}

/// Answer a `/bridge` request: refuse it, or upgrade it and serve the tab until it goes. The
/// connection is the tab's from here on.
pub fn serve(relay: &crate::Relay, req: &Request, reader: BufReader<TcpStream>, peer: &str) -> Option<Response> {
    if req.method != "GET" || !req.header_has("upgrade", "websocket") || !req.header_has("connection", "upgrade") {
        return Some(Response::text(426, "the bridge is a WebSocket: the SolidCraft web app connects to it").header("Upgrade", "websocket"));
    }
    if req.header("sec-websocket-version") != Some("13") {
        return Some(Response::text(426, "WebSocket version 13 only").header("Sec-WebSocket-Version", "13"));
    }
    let Some(key) = req.header("sec-websocket-key").filter(|k| !k.is_empty() && k.len() <= 64) else {
        return Some(Response::text(400, "no Sec-WebSocket-Key"));
    };
    // A browser always says where the page is from; only pages from the relay's own site (or
    // one allowed by name) may connect a tab.
    if let Some(origin) = req.header("origin")
        && !origin_allowed(origin, req, &relay.config.allowed_origins)
    {
        let host = req.header("x-forwarded-host").or_else(|| req.header("host")).unwrap_or("?");
        eprintln!(
            "solidcraft-relay: refused a tab from {peer}: its page's origin {origin} is not this site ({host}); set SOLIDCRAFT_RELAY_ALLOWED_ORIGIN \
             to allow it"
        );
        return Some(Response::text(403, "this page's origin may not connect to the relay (SOLIDCRAFT_RELAY_ALLOWED_ORIGIN)"));
    }
    if relay.hub.bridges.fetch_add(1, Ordering::Relaxed) >= MAX_BRIDGES {
        relay.hub.bridges.fetch_sub(1, Ordering::Relaxed);
        return Some(Response::text(503, "too many bridge connections").closing());
    }
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let leftover = reader.buffer().to_vec();
    let mut stream = reader.into_inner();
    let upgraded = Response::new(101)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept)
        .write_upgrade(&mut stream);
    if upgraded.is_ok() {
        let config = WebSocketConfig::default().max_message_size(Some(MAX_REPLY)).max_frame_size(Some(MAX_REPLY));
        let ws = WebSocket::from_partially_read(stream, leftover, Role::Server, Some(config));
        run(relay, ws, peer, req.header("origin").unwrap_or("no origin"));
    }
    relay.hub.bridges.fetch_sub(1, Ordering::Relaxed);
    None
}

fn close(ws: &mut WebSocket<TcpStream>, code: u16, reason: &str) {
    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() }));
    // Drive the close a little (the tab's answer), then drop the connection.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
}

/// One message read, or `None` when nothing came in this short wait.
enum Read {
    Text(String),
    Nothing,
    Other,
    Gone(String),
}

fn read(ws: &mut WebSocket<TcpStream>) -> Read {
    match ws.read() {
        Ok(Message::Text(t)) => Read::Text(t.as_str().to_string()),
        Ok(Message::Close(_)) => Read::Gone("it closed the connection".into()),
        Ok(_) => Read::Other,
        Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Read::Nothing,
        Err(tungstenite::Error::Capacity(e)) => {
            close(ws, 1009, "message too large");
            Read::Gone(format!("it sent too much ({e})"))
        }
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => Read::Gone("the connection closed".into()),
        Err(e) => Read::Gone(e.to_string()),
    }
}

/// The token check: the first message is `{"type": "hello", "token": …}`.
fn sign_in(relay: &crate::Relay, ws: &mut WebSocket<TcpStream>, peer: &str) -> bool {
    let deadline = Instant::now() + AUTH_WAIT;
    loop {
        if Instant::now() >= deadline {
            close(ws, CLOSE_UNAUTHORIZED, "unauthorized: no token");
            eprintln!("solidcraft-relay: a tab from {peer} showed no token in time");
            return false;
        }
        match read(ws) {
            Read::Nothing | Read::Other => continue,
            Read::Gone(_) => return false,
            Read::Text(t) => {
                let hello: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                let token = hello.get("token").and_then(Value::as_str).unwrap_or("");
                if hello.get("type").and_then(Value::as_str) == Some("hello")
                    && solidcraft_engine::control_auth::tokens_match(token, &relay.config.token)
                {
                    return true;
                }
                close(ws, CLOSE_UNAUTHORIZED, "unauthorized: wrong token");
                eprintln!("solidcraft-relay: refused a tab from {peer}: wrong token");
                return false;
            }
        }
    }
}

fn run(relay: &crate::Relay, mut ws: WebSocket<TcpStream>, peer: &str, origin: &str) {
    // Short reads, so requests for the tab go out as soon as they come.
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
    let _ = ws.get_ref().set_write_timeout(Some(Duration::from_secs(30)));
    if !sign_in(relay, &mut ws, peer) {
        return;
    }
    let (number, outgoing) = relay.hub.register();
    let welcome = json!({"type": "welcome", "tab": number, "relay": env!("CARGO_PKG_VERSION")});
    if ws.send(Message::text(welcome.to_string())).is_err() {
        relay.hub.unregister(number);
        return;
    }
    eprintln!("solidcraft-relay: tab {number} connected from {peer} ({origin})");
    let why = serve_tab(relay, &mut ws, number, &outgoing);
    relay.hub.unregister(number);
    eprintln!("solidcraft-relay: tab {number} disconnected: {why}");
}

fn serve_tab(relay: &crate::Relay, ws: &mut WebSocket<TcpStream>, number: u64, outgoing: &Receiver<Outgoing>) -> String {
    let (mut heard, mut pinged) = (Instant::now(), Instant::now());
    let mut bad = 0usize;
    loop {
        for out in outgoing.try_iter() {
            match out {
                Outgoing::Text(t) => {
                    if let Err(e) = ws.send(Message::text(t)) {
                        return format!("sending failed ({e})");
                    }
                }
                Outgoing::Close(code, reason) => {
                    close(ws, code, &reason);
                    return reason;
                }
            }
        }
        if pinged.elapsed() >= relay.ping_every {
            pinged = Instant::now();
            if ws.send(Message::text(r#"{"type":"ping"}"#)).is_err() {
                return "the connection broke".into();
            }
        }
        if heard.elapsed() >= relay.silence {
            close(ws, 1001, "no heartbeat");
            return format!("nothing heard for {} s", relay.silence.as_secs());
        }
        match read(ws) {
            Read::Nothing => {}
            Read::Other => heard = Instant::now(),
            Read::Gone(why) => return why,
            Read::Text(t) => {
                heard = Instant::now();
                let Ok(msg) = serde_json::from_str::<Value>(&t) else {
                    bad += 1;
                    if bad >= MAX_BAD_MESSAGES {
                        close(ws, 1007, "not JSON");
                        return "it sent messages that are not JSON".into();
                    }
                    continue;
                };
                match msg.get("type").and_then(Value::as_str) {
                    Some("ping") => {
                        let _ = ws.send(Message::text(r#"{"type":"pong"}"#));
                    }
                    Some(_) => {}
                    None => relay.hub.deliver(number, msg),
                }
            }
        }
    }
}
