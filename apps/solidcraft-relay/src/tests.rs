//! The relay end to end on a real socket: agents over HTTP, a pretend tab over a WebSocket.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use solidcraft_mcp::{HeadlessMode, TabWaits};
use tungstenite::client::IntoClientRequest as _;
use tungstenite::{Message, WebSocket};

use crate::config::Config;
use crate::{Relay, serve};

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

fn config(headless: HeadlessMode) -> Config {
    let mut c = Config::from_env(|k| (k == "SOLIDCRAFT_RELAY_TOKEN").then(|| TOKEN.to_string())).unwrap();
    c.headless = headless;
    c.data = std::env::temp_dir().join(format!("solidcraft-relay-test-{}", std::process::id()));
    c.waits = TabWaits { reply: Duration::from_secs(10), batch: Duration::from_secs(10), settle: Duration::from_millis(200) };
    c
}

fn start(config: Config) -> (SocketAddr, Arc<Relay>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = Arc::new(Relay::new(config).unwrap());
    let r = relay.clone();
    std::thread::spawn(move || serve(listener, r));
    (addr, relay)
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// One HTTP request on a fresh connection.
fn http(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut headers = Vec::new();
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).unwrap();
    Reply { status, headers, body }
}

const AUTH: (&str, &str) = ("Authorization", "Bearer test-token-0123456789abcdef0123456789");

/// An agent's MCP session.
struct Agent {
    addr: SocketAddr,
    session: String,
    next: u64,
}

impl Agent {
    fn connect(addr: SocketAddr) -> Self {
        let r = rpc_raw(
            addr,
            None,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}),
        );
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        assert_eq!(r.json()["result"]["protocolVersion"], "2025-06-18");
        let session = r.header("mcp-session-id").unwrap().to_string();
        let mut a = Agent { addr, session, next: 2 };
        let n = a.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        assert_eq!((n.status, n.body.len()), (202, 0));
        a
    }
    fn post(&mut self, msg: &Value) -> Reply {
        rpc_raw(self.addr, Some(&self.session), msg)
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let r = self.post(&json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        r.json()
    }
    /// A tool's result: `(is_error, first text, all content)`.
    fn tool(&mut self, name: &str, args: Value) -> (bool, String, Value) {
        let r = self.call("tools/call", json!({"name": name, "arguments": args}));
        let content = r["result"]["content"].clone();
        let text = content[0]["text"].as_str().unwrap_or_default().to_string();
        (r["result"]["isError"] == true, text, content)
    }
}

fn rpc_raw(addr: SocketAddr, session: Option<&str>, msg: &Value) -> Reply {
    let mut headers = vec![AUTH, ("Content-Type", "application/json"), ("Accept", "application/json, text/event-stream")];
    if let Some(s) = session {
        headers.push(("Mcp-Session-Id", s));
        headers.push(("MCP-Protocol-Version", "2025-06-18"));
    }
    http(addr, "POST", "/mcp", &headers, msg.to_string().as_bytes())
}

type Ws = WebSocket<TcpStream>;

/// A pretend tab: opens the bridge as the web app does (`origin` as its page's).
fn tab_with(addr: SocketAddr, origin: Option<&str>, token: &str) -> Result<Ws, String> {
    let stream = TcpStream::connect(addr).unwrap();
    let mut request = format!("ws://{addr}/bridge").into_client_request().unwrap();
    if let Some(o) = origin {
        request.headers_mut().insert("Origin", o.parse().unwrap());
    }
    let (mut ws, _) = tungstenite::client(request, stream).map_err(|e| e.to_string())?;
    ws.get_ref().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    ws.send(Message::text(json!({"type": "hello", "token": token}).to_string())).unwrap();
    Ok(ws)
}

fn tab(addr: SocketAddr) -> Ws {
    let mut ws = tab_with(addr, Some(&format!("http://{addr}")), TOKEN).unwrap();
    let welcome = next(&mut ws);
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    ws
}

/// The next JSON message, answering pings on the way.
fn next(ws: &mut Ws) -> Value {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "ping" {
                    ws.send(Message::text(r#"{"type":"pong"}"#)).unwrap();
                    continue;
                }
                return v;
            }
            Ok(Message::Close(f)) => return json!({"closed": f.map(|f| u16::from(f.code))}),
            Ok(_) => {}
            Err(e) => return json!({"error": e.to_string()}),
        }
    }
}

/// Answer the tab's next request with `answer`; returns the request.
fn answer(ws: &mut Ws, answer: impl FnOnce(&Value) -> Value) -> Value {
    let req = next(ws);
    let mut reply = answer(&req);
    reply["id"] = req["id"].clone();
    ws.send(Message::text(reply.to_string())).unwrap();
    req
}

/// Wait until the relay counts a connected tab (or none).
fn wait_tab(relay: &Relay, connected: bool) {
    use solidcraft_mcp::TabLink as _;
    for _ in 0..500 {
        if relay.hub.connection().is_some() == connected {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the tab is not {}", if connected { "connected" } else { "gone" });
}

#[test]
fn agents_need_the_token() {
    let (addr, _) = start(config(HeadlessMode::Off));
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string();
    for headers in [vec![], vec![("Authorization", "Bearer wrong-token-0123456789abcdef0123456789")], vec![("Authorization", "Basic dGVzdA==")]] {
        let r = http(addr, "POST", "/mcp", &headers, init.as_bytes());
        assert_eq!(r.status, 401, "{headers:?}");
        assert!(r.header("www-authenticate").is_some_and(|h| h.starts_with("Bearer")));
        assert!(!String::from_utf8_lossy(&r.body).contains(TOKEN));
    }
    assert_eq!(http(addr, "GET", "/mcp", &[], b"").status, 401);
    // Refused from its head: the body of a request without the token is never read.
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(format!("POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", solidcraft_engine::MAX_LINE).as_bytes()).unwrap();
    let mut status = String::new();
    BufReader::new(s).read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 401"), "{status}");
    assert_eq!(http(addr, "POST", "/mcp", &[AUTH], init.as_bytes()).status, 200);
    assert_eq!(http(addr, "POST", "/mcp", &[("authorization", "bearer test-token-0123456789abcdef0123456789")], init.as_bytes()).status, 200);
    // Not secret: the health check, and what isn't there.
    assert_eq!(http(addr, "GET", "/healthz", &[], b"").status, 200);
    assert_eq!(http(addr, "GET", "/.well-known/oauth-protected-resource", &[], b"").status, 404);
}

#[test]
fn the_mcp_endpoint_speaks_streamable_http() {
    let (addr, _) = start(config(HeadlessMode::Off));
    let mut agent = Agent::connect(addr);
    let tools = agent.call("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names.len(), solidcraft_mcp::tool_definitions().len());
    for t in ["execute", "batch", "inspect_design", "screenshot", "pending", "open", "save", "describe_command", "skill_reference"] {
        assert!(names.contains(&t), "{t} missing from {names:?}");
    }
    assert_eq!(agent.call("ping", json!({}))["result"], json!({}));
    // A batch: the answers to its requests.
    let r = agent.post(&json!([{"jsonrpc": "2.0", "id": 7, "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/cancelled"}]));
    assert_eq!((r.status, r.json()[0]["id"].clone()), (200, json!(7)));
    // GET opens no event stream.
    let r = http(addr, "GET", "/mcp", &[AUTH, ("Mcp-Session-Id", &agent.session)], b"");
    assert_eq!((r.status, r.header("allow")), (405, Some("POST, DELETE")));
    // Sessions: needed, known, ended.
    let ping = json!({"jsonrpc": "2.0", "id": 9, "method": "ping"});
    assert_eq!(rpc_raw(addr, None, &ping).status, 400);
    assert_eq!(rpc_raw(addr, Some("0000"), &ping).status, 404);
    let r =
        http(addr, "POST", "/mcp", &[AUTH, ("Mcp-Session-Id", &agent.session), ("MCP-Protocol-Version", "1999-01-01")], ping.to_string().as_bytes());
    assert_eq!(r.status, 400);
    assert_eq!(http(addr, "POST", "/mcp", &[AUTH, ("Mcp-Session-Id", &agent.session)], b"{not json").status, 400);
    assert_eq!(http(addr, "POST", "/mcp", &[AUTH, ("Mcp-Session-Id", &agent.session), ("Content-Type", "text/plain")], b"{}").status, 415);
    assert_eq!(http(addr, "DELETE", "/mcp", &[AUTH, ("Mcp-Session-Id", &agent.session)], b"").status, 204);
    assert_eq!(agent.post(&ping).status, 404);
}

#[test]
fn without_a_tab_tool_calls_say_how_to_connect_one() {
    let (addr, _) = start(config(HeadlessMode::Off));
    let mut agent = Agent::connect(addr);
    let (error, text, _) = agent.tool("execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    assert!(error && text.starts_with("no SolidCraft tab is connected") && text.contains(&format!("http://{addr}/#mcp=<token>")), "{text}");
}

#[test]
fn a_tab_answers_the_agents_tool_calls() {
    let (addr, relay) = start(config(HeadlessMode::Off));
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    let mut agent = Agent::connect(addr);
    let t = std::thread::spawn(move || {
        let req = answer(&mut ws, |_| json!({"ok": true, "result": {"feature": 1, "created_bodies": ["Body1"]}}));
        let shot = answer(
            &mut ws,
            |_| json!({"ok": true, "result": {"width": 1, "height": 1, "png_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg=="}}),
        );
        (req, shot, ws)
    });
    let (error, text, _) = agent.tool("execute", json!({"command": "solid.box", "params": {"length": 30}}));
    assert!(!error && text.contains("Body1"), "{text}");
    let (error, _, content) = agent.tool("screenshot", json!({}));
    assert!(!error && content[0]["type"] == "image" && content[0]["mimeType"] == "image/png", "{content}");
    let (req, shot, _ws) = t.join().unwrap();
    assert_eq!(
        (&req["method"], &req["params"]["command"], &req["params"]["params"]),
        (&json!("engine.execute"), &json!("solid.box"), &json!({"length": 30}))
    );
    assert_eq!((&shot["method"], &shot["params"]), (&json!("ui.screenshot"), &json!({"inline": true})));
}

#[test]
fn a_tab_needs_the_token_and_the_sites_origin() {
    let (addr, relay) = start(config(HeadlessMode::Off));
    let mut ws = tab_with(addr, Some(&format!("http://{addr}")), "wrong-token-0123456789abcdef0123456789").unwrap();
    assert_eq!(next(&mut ws)["closed"], crate::bridge::CLOSE_UNAUTHORIZED);
    let e = tab_with(addr, Some("https://evil.example"), TOKEN).err().unwrap();
    assert!(e.contains("403"), "{e}");
    // A client that is no browser says no origin: the token alone.
    let mut ws = tab_with(addr, None, TOKEN).unwrap();
    assert_eq!(next(&mut ws)["type"], "welcome");
    wait_tab(&relay, true);
    // An origin allowed by name.
    let mut c = config(HeadlessMode::Off);
    c.allowed_origins = vec!["https://cad.example".into()];
    let (addr, _) = start(c);
    let mut ws = tab_with(addr, Some("https://CAD.example/"), TOKEN).unwrap();
    assert_eq!(next(&mut ws)["type"], "welcome");
    assert!(tab_with(addr, Some(&format!("http://{addr}")), TOKEN).err().unwrap().contains("403"));
}

#[test]
fn the_newest_tab_wins() {
    let (addr, relay) = start(config(HeadlessMode::Off));
    let mut first = tab(addr);
    wait_tab(&relay, true);
    let mut second = tab(addr);
    let closed = next(&mut first);
    assert_eq!(closed["closed"], crate::bridge::CLOSE_REPLACED, "{closed}");
    let mut agent = Agent::connect(addr);
    let t = std::thread::spawn(move || answer(&mut second, |_| json!({"ok": true, "result": {"from": "second"}})));
    let (error, text, _) = agent.tool("execute", json!({"command": "edit.undo"}));
    assert!(!error && text.contains("second"), "{text}");
    t.join().unwrap();
}

#[test]
fn a_request_that_outlasts_its_wait_is_left_to_pending() {
    let mut c = config(HeadlessMode::Off);
    c.waits = TabWaits { reply: Duration::from_millis(300), batch: Duration::from_millis(300), settle: Duration::from_secs(5) };
    let (addr, relay) = start(c);
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    let mut agent = Agent::connect(addr);
    let req = std::thread::spawn(move || {
        let req = next(&mut ws);
        (req, ws)
    });
    let (error, text, _) = agent.tool("execute", json!({"command": "solid.fillet"}));
    assert!(error && text.starts_with("still running:"), "{text}");
    let (req, mut ws) = req.join().unwrap();
    let (_, running, _) = agent.tool("running", json!({}));
    assert!(running.contains("solid.fillet"), "{running}");
    ws.send(Message::text(json!({"id": req["id"], "ok": true, "result": {"filleted": true}}).to_string())).unwrap();
    let (error, text, _) = agent.tool("pending", json!({}));
    assert!(!error && text.contains("filleted") && text.contains("\"done\": true"), "{text}");
}

#[test]
fn a_tab_that_goes_away_loses_its_requests() {
    let (addr, relay) = start(config(HeadlessMode::Off));
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    let mut agent = Agent::connect(addr);
    let t = std::thread::spawn(move || {
        let _ = next(&mut ws);
        drop(ws);
    });
    let (error, text, _) = agent.tool("execute", json!({"command": "solid.box"}));
    t.join().unwrap();
    assert!(error && text.contains("went away"), "{text}");
    wait_tab(&relay, false);
}

#[test]
fn a_silent_tab_is_dropped() {
    let mut relay = Relay::new(config(HeadlessMode::Off)).unwrap();
    relay.ping_every = Duration::from_millis(50);
    relay.silence = Duration::from_millis(300);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = Arc::new(relay);
    let r = relay.clone();
    std::thread::spawn(move || serve(listener, r));
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    // It reads (the pings arrive) but never answers.
    let mut pings = 0;
    while let Ok(m) = ws.read() {
        if let Message::Text(t) = m
            && t.as_str().contains("ping")
        {
            pings += 1;
        }
    }
    assert!(pings >= 2, "{pings}");
    wait_tab(&relay, false);
}

#[test]
fn oversized_messages_are_refused() {
    let (addr, relay) = start(config(HeadlessMode::Off));
    // Refused from its length, before the body is read.
    let mut s = TcpStream::connect(addr).unwrap();
    let head =
        format!("POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n", solidcraft_engine::MAX_LINE + 1);
    s.write_all(head.as_bytes()).unwrap();
    let mut status = String::new();
    BufReader::new(s).read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 413"), "{status}");
    // A tab may send a reply up to MAX_REPLY; one larger ends its connection.
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    let huge = format!("{{\"id\": 1, \"ok\": true, \"result\": \"{}\"}}", "x".repeat(crate::bridge::MAX_REPLY));
    let _ = ws.send(Message::text(huge));
    wait_tab(&relay, false);
}

#[test]
fn the_headless_fallback_answers_while_no_tab_is_connected() {
    let (addr, relay) = start(config(HeadlessMode::Fallback));
    let mut agent = Agent::connect(addr);
    let (error, text, content) = agent.tool("execute", json!({"command": "solid.box", "params": {"length": 10, "width": 10, "height": 10}}));
    assert!(!error, "{text}");
    assert!(content.as_array().unwrap().iter().any(|c| c["text"].as_str().is_some_and(|t| t.contains("headless session"))), "{content}");
    let (error, _, content) = agent.tool("screenshot", json!({"width": 64, "height": 48}));
    assert!(!error && content[0]["type"] == "image", "{content}");
    // A tab connects: the calls go to it.
    let mut ws = tab(addr);
    wait_tab(&relay, true);
    let t = std::thread::spawn(move || answer(&mut ws, |_| json!({"ok": true, "result": {"from": "tab"}})));
    let (error, text, _) = agent.tool("execute", json!({"command": "edit.undo"}));
    assert!(!error && text.contains("tab"), "{text}");
    t.join().unwrap();
}

/// Agents keep their connection open between requests (HTTP/1.1 keep-alive).
#[test]
fn one_connection_carries_many_requests() {
    let (addr, _) = start(config(HeadlessMode::Off));
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut out = s.try_clone().unwrap();
    let mut r = BufReader::new(s);
    let mut send = |body: &str, session: Option<&str>| -> (u16, Vec<(String, String)>, Vec<u8>) {
        let session = session.map(|s| format!("Mcp-Session-Id: {s}\r\n")).unwrap_or_default();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\n\r\n",
            body.len()
        );
        out.write_all(head.as_bytes()).unwrap();
        out.write_all(body.as_bytes()).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let status = line.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            r.read_line(&mut line).unwrap();
            match line.trim_end().split_once(':') {
                Some((k, v)) => headers.push((k.to_ascii_lowercase(), v.trim().to_string())),
                None => break,
            }
        }
        let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        (status, headers, body)
    };
    let (status, headers, _) = send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#, None);
    assert_eq!(status, 200);
    assert!(headers.iter().any(|(k, v)| k == "connection" && v == "keep-alive"));
    let session = headers.iter().find(|(k, _)| k == "mcp-session-id").map(|(_, v)| v.clone()).unwrap();
    assert_eq!(send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, Some(&session)).0, 202);
    for id in 2..5 {
        let (status, _, body) = send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#), Some(&session));
        assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()["id"].clone()), (200, json!(id)));
    }
}
