//! `cargo xtask web-mcp-test [--site DIR] [--relay PATH] [--out DIR]`: an agent drives the web app
//! in a real browser tab through `solidcraft-relay` (docs/web-mcp.md).
//!
//! Starts the relay, a small server for the web build that passes `/bridge` on to the relay (as
//! nginx does in packaging/docker), and headless Chrome on the app with `#mcp=TOKEN`. Then, as
//! an MCP client over Streamable HTTP: no token and a wrong one are refused; the tab connects by
//! itself, and the token leaves the address; `execute solid.box` → `inspect_design` (one body,
//! 6000 mm³) → `screenshot` (the window and a model render, written to `--out` to look at) →
//! `batch` → `pending` → `save` and `open` in the browser's storage; without a tab a call says so;
//! a second tab takes over from the first. A picture of the page with its status chip is kept too.
//! The relay is built with the ci profile unless `--relay` names one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::web_offline_test::{Cdp, Chrome, LAUNCH_TIMEOUT, TempDir, find_chrome, wait_for};

/// The relay's token for this run (the test's only secret, and only for itself).
fn token() -> String {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("e2e-{seed:032x}-{:08x}", std::process::id())
}

struct Relay {
    child: Child,
    port: u16,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

/// Build the relay (ci profile) and say where it is.
fn build_relay(root: &Path) -> Result<PathBuf, String> {
    let mut c = crate::cargo();
    c.args(["build", "--profile", "ci", "-p", "solidcraft-relay"]);
    crate::run(c, "cargo build -p solidcraft-relay")?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let exe = target.join("ci").join(if cfg!(windows) { "solidcraft-relay.exe" } else { "solidcraft-relay" });
    exe.is_file().then_some(exe.clone()).ok_or_else(|| format!("{}: not built", exe.display()))
}

fn start_relay(exe: &Path, token: &str, data: &Path) -> Result<Relay, String> {
    let port = free_port()?;
    let child = Command::new(exe)
        .env("SOLIDCRAFT_RELAY_TOKEN", token)
        .env("SOLIDCRAFT_RELAY_LISTEN", format!("127.0.0.1:{port}"))
        .env("SOLIDCRAFT_RELAY_DATA", data)
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let relay = Relay { child, port };
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) {
        if http(port, "GET", "/healthz", &[], b"").is_ok_and(|r| r.0 == 200) {
            return Ok(relay);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("the relay didn't start within 30 s".into())
}

/// The site at `/`, and `/bridge` passed on to the relay byte for byte.
fn start_site(site: PathBuf, relay: u16) -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("site server: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let site = site.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, &site, relay);
            });
        }
    });
    Ok(port)
}

fn answer(stream: TcpStream, site: &Path, relay: u16) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let n = reader.read_until(b'\n', &mut head)?;
        if n == 0 || head.len() > 16 * 1024 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let path = text.split_whitespace().nth(1).unwrap_or("/").split(['?', '#']).next().unwrap_or("/").to_string();
    let mut out = stream;
    if path == "/bridge" {
        // As nginx's `location = /bridge`: the upgrade and everything after it go to the relay.
        let mut upstream = TcpStream::connect(("127.0.0.1", relay))?;
        upstream.write_all(&head)?;
        upstream.write_all(reader.buffer())?;
        let (mut up_read, mut down_write) = (upstream.try_clone()?, out.try_clone()?);
        let pump = std::thread::spawn(move || {
            let _ = std::io::copy(&mut up_read, &mut down_write);
            let _ = down_write.shutdown(std::net::Shutdown::Both);
        });
        let mut client = reader.into_inner();
        let _ = std::io::copy(&mut client, &mut upstream);
        let _ = upstream.shutdown(std::net::Shutdown::Both);
        let _ = pump.join();
        return Ok(());
    }
    let file = path.trim_start_matches('/');
    let file = if file.is_empty() { "index.html" } else { file };
    let body = (!file.contains("..")).then(|| std::fs::read(site.join(file)).ok()).flatten();
    let kind = match file.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    let (status, body) = match body {
        Some(b) => ("200 OK", b),
        None => ("404 Not Found", b"not found".to_vec()),
    };
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(&body)
}

/// One HTTP/1.1 request on a new connection: status, headers (lower-case names) and body.
type Reply = (u16, Vec<(String, String)>, Vec<u8>);

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Reply, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("relay: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(600))).map_err(|e| e.to_string())?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).and_then(|()| s.write_all(body)).map_err(|e| format!("relay: {e}"))?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| format!("relay: {e}"))?;
    let status = line.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| format!("relay: bad status line {line:?}"))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        r.read_line(&mut line).map_err(|e| format!("relay: {e}"))?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).map_err(|e| format!("relay: {e}"))?;
    Ok((status, headers, body))
}

/// An MCP client session with the relay.
struct Agent {
    port: u16,
    auth: String,
    session: String,
    next: u64,
}

impl Agent {
    fn connect(port: u16, token: &str) -> Result<Agent, String> {
        let auth = format!("Bearer {token}");
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "web-mcp-test", "version": "1"}}});
        let headers = [("Authorization", auth.as_str()), ("Content-Type", "application/json"), ("Accept", "application/json, text/event-stream")];
        let (status, h, body) = http(port, "POST", "/mcp", &headers, init.to_string().as_bytes())?;
        if status != 200 {
            return Err(format!("initialize: HTTP {status}: {}", String::from_utf8_lossy(&body)));
        }
        let session = h.iter().find(|(k, _)| k == "mcp-session-id").map(|(_, v)| v.clone()).ok_or("initialize: no Mcp-Session-Id")?;
        let agent = Agent { port, auth, session, next: 1 };
        let (status, _, _) = agent.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        if status != 202 {
            return Err(format!("notifications/initialized: HTTP {status}, not 202"));
        }
        Ok(agent)
    }

    fn post(&self, msg: &Value) -> Result<Reply, String> {
        let headers = [
            ("Authorization", self.auth.as_str()),
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
            ("Mcp-Session-Id", self.session.as_str()),
            ("MCP-Protocol-Version", "2025-06-18"),
        ];
        http(self.port, "POST", "/mcp", &headers, msg.to_string().as_bytes())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let (status, _, body) = self.post(&json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}))?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("{method}: HTTP {status}: {e}"))?;
        if status != 200 || v.get("error").is_some() {
            return Err(format!("{method}: HTTP {status}: {v}"));
        }
        Ok(v["result"].clone())
    }

    /// A tool call's content, or its error text.
    fn tool(&mut self, name: &str, args: Value) -> Result<Vec<Value>, String> {
        let r = self.call("tools/call", json!({"name": name, "arguments": args}))?;
        let content = r["content"].as_array().cloned().unwrap_or_default();
        let text = content.first().and_then(|c| c["text"].as_str()).unwrap_or("").to_string();
        if r["isError"] == true { Err(text) } else { Ok(content) }
    }

    /// A tool's JSON answer (its first text).
    fn json(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let content = self.tool(name, args)?;
        let text = content.first().and_then(|c| c["text"].as_str()).ok_or_else(|| format!("{name}: no text"))?;
        serde_json::from_str(text).map_err(|e| format!("{name}: {e}: {text}"))
    }
}

fn check(ok: bool, what: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(format!("web-mcp-test: {what}")) }
}

/// Decode standard base64 (the screenshot's PNG).
fn unbase64(text: &str) -> Result<Vec<u8>, String> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("not base64".to_string()),
    };
    let mut out = Vec::new();
    for chunk in text.trim_end_matches('=').as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= u32::from(value(*c)?) << (18 - 6 * i as u32);
        }
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..chunk.len().saturating_sub(1)]);
    }
    Ok(out)
}

/// Keep a screenshot's PNG in `out`; its size from the PNG header.
fn keep_png(content: &[Value], out: &Path, name: &str) -> Result<(u32, u32), String> {
    let image = content.iter().find(|c| c["type"] == "image").ok_or("screenshot: no image")?;
    check(image["mimeType"] == "image/png", "screenshot is a PNG")?;
    let png = unbase64(image["data"].as_str().unwrap_or(""))?;
    check(png.starts_with(b"\x89PNG\r\n\x1a\n") && png.len() > 24, "screenshot decodes to a PNG")?;
    let size = |at: usize| png.get(at..at + 4).and_then(|b| b.try_into().ok()).map_or(0, u32::from_be_bytes);
    let path = out.join(name);
    std::fs::write(&path, &png).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("web-mcp-test: kept {} ({} × {})", path.display(), size(16), size(20));
    Ok((size(16), size(20)))
}

const STATUS: &str = "(() => { const a = window.solidcraft && window.solidcraft.agent; return a ? a.status() : null; })()";

/// Wait until the tab's bridge is in `state`.
fn wait_state(cdp: &mut Cdp, state: &str) -> Result<(), String> {
    wait_for(cdp, STATUS, &format!("the bridge is {state}"), |s| s["state"] == state)
}

pub fn run(root: &Path, site: &str, relay: Option<&str>, out: Option<&str>) -> Result<(), String> {
    let site = std::fs::canonicalize(site).map_err(|e| format!("{site}: {e}; build apps/solidcraft-web with trunk first"))?;
    if !site.join("index.html").is_file() {
        return Err(format!("{}: index.html missing; build apps/solidcraft-web with trunk first", site.display()));
    }
    let chrome = find_chrome().ok_or("web-mcp-test: Chrome not found (set CHROME to its executable)")?;
    let exe = match relay {
        Some(p) => PathBuf::from(p),
        None => build_relay(root)?,
    };
    let work = TempDir::named("solidcraft-web-mcp-test")?;
    let out = match out {
        Some(o) => PathBuf::from(o),
        None => work.0.join("out"),
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let token = token();
    let relay = start_relay(&exe, &token, &work.0.join("data"))?;
    let port = start_site(site, relay.port)?;

    // Agents need the token.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string();
    let (status, _, _) = http(relay.port, "POST", "/mcp", &[], init.as_bytes())?;
    check(status == 401, "no token: 401")?;
    let (status, _, _) = http(relay.port, "POST", "/mcp", &[("Authorization", "Bearer not-the-token-0123456789")], init.as_bytes())?;
    check(status == 401, "a wrong token: 401")?;
    let mut agent = Agent::connect(relay.port, &token)?;
    let tools = agent.call("tools/list", json!({}))?;
    let names: Vec<String> = tools["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
    for t in ["describe_command", "execute", "batch", "inspect_design", "screenshot", "pending", "open", "save", "skill_reference"] {
        check(names.iter().any(|n| n == t), &format!("tools/list has {t}"))?;
    }
    println!("web-mcp-test: {} tools", names.len());
    // No tab yet.
    let e = agent.tool("execute", json!({"command": "solid.box"})).err().unwrap_or_default();
    check(e.starts_with("no SolidCraft tab is connected"), &format!("without a tab the call says so ({e})"))?;

    // The tab connects by itself, and the token leaves the address.
    let browser = Chrome::launch(&chrome, &work.0.join("profile"))?;
    let mut cdp = browser.connect()?;
    cdp.call("Emulation.setDeviceMetricsOverride", json!({"width": 1280, "height": 860, "deviceScaleFactor": 1, "mobile": false}))?;
    cdp.call("Emulation.setFocusEmulationEnabled", json!({"enabled": true}))?;
    cdp.call("Page.navigate", json!({"url": format!("http://127.0.0.1:{port}/?webgl#mcp={token}")}))?;
    wait_for(&mut cdp, "!!window.solidcraft && !document.getElementById('solidcraft_loading')", "the app started", |v| v == true)?;
    wait_state(&mut cdp, "connected")?;
    let address = cdp.eval("[location.hash, localStorage.getItem('solidcraft.mcp.token') !== null]")?;
    check(address == json!(["", true]), &format!("the token left the address and stays in the browser ({address})"))?;
    cdp.eval("solidcraft.request('ui.home', {open: false})")?;

    // Model, inspect, look.
    let r = agent.json("execute", json!({"command": "solid.box", "params": {"length": 30, "width": 20, "height": 10, "body_name": "Agent box"}}))?;
    check(r["created_bodies"].as_array().is_some_and(|b| b.len() == 1), &format!("solid.box made a body ({r})"))?;
    let d = agent.json("inspect_design", json!({}))?;
    let volume = d["bodies"][0]["volume_mm3"].as_f64().unwrap_or(0.0);
    check(
        d["bodies"].as_array().map(Vec::len) == Some(1) && (volume - 6000.0).abs() < 1e-3,
        &format!("inspect_design: one body of 6000 mm³ ({volume})"),
    )?;
    let in_tab = cdp.eval("solidcraft.inspect({measure: true}).then(d => d.bodies.length)")?;
    check(in_tab == json!(1), "the box is in the tab's design")?;
    let shot = agent.tool("screenshot", json!({"view": "iso"}))?;
    let (w, h) = keep_png(&shot, &out, "window.png")?;
    check(w >= 600 && h >= 400, "the window screenshot is the page's size")?;
    let info: Value = serde_json::from_str(shot.get(1).and_then(|c| c["text"].as_str()).unwrap_or("{}")).unwrap_or(Value::Null);
    check(info["source"] == "window", &format!("a window screenshot ({info})"))?;
    let model = agent.tool("screenshot", json!({"source": "model", "width": 640, "height": 400, "path": "/opfs/e2e-shot.png"}))?;
    check(keep_png(&model, &out, "model.png")? == (640, 400), "the model render has the size asked for")?;
    // Batch, pending.
    let b = agent.json("batch", json!({"commands": [{"command": "edit.undo"}, {"command": "edit.redo"}]}))?;
    check(b["ok"] == true && b["completed"] == 2, &format!("batch ran both steps ({b})"))?;
    let p = agent.json("pending", json!({}))?;
    check(p["pending"].is_null(), &format!("nothing pending ({p})"))?;
    // Save and open in the browser's storage.
    agent.json("save", json!({"path": "/opfs/e2e.solidcraft"}))?;
    agent.json("new_design", json!({}))?;
    check(agent.json("inspect_design", json!({}))?["bodies"].as_array().is_some_and(Vec::is_empty), "a new design is empty")?;
    agent.json("open", json!({"path": "/opfs/e2e.solidcraft"}))?;
    let reopened = wait_until(|| Ok(agent.json("inspect_design", json!({}))?["bodies"].as_array().map_or(0, Vec::len) == 1))?;
    check(reopened, "open brings the saved box back")?;
    agent.json("screenshot", json!({"view": "iso"})).ok();
    let page = cdp.call("Page.captureScreenshot", json!({"format": "png"}))?;
    let page_png = unbase64(page["data"].as_str().unwrap_or(""))?;
    std::fs::write(out.join("page.png"), &page_png).map_err(|e| e.to_string())?;
    println!("web-mcp-test: kept {}", out.join("page.png").display());

    // Without a tab, calls say so; connect() brings it back.
    cdp.eval("solidcraft.agent.disconnect()")?;
    wait_until(|| Ok(agent.tool("execute", json!({"command": "edit.undo"})).err().is_some_and(|e| e.starts_with("no SolidCraft tab is connected"))))?
        .then_some(())
        .ok_or("after disconnect() calls say no tab is connected")?;
    cdp.eval(&format!("solidcraft.agent.connect({})", json!(token)))?;
    wait_state(&mut cdp, "connected")?;
    // A second tab takes over; the first is told, and doesn't come back by itself.
    let mut second = browser.new_tab()?;
    second.call("Page.navigate", json!({"url": format!("http://127.0.0.1:{port}/?webgl")}))?;
    wait_for(&mut second, "!!window.solidcraft && !document.getElementById('solidcraft_loading')", "the second tab started", |v| v == true)?;
    wait_state(&mut second, "connected")?;
    wait_state(&mut cdp, "replaced")?;
    let r = agent.call("tools/call", json!({"name": "inspect_design", "arguments": {}}))?;
    let notice = r["content"].as_array().into_iter().flatten().any(|c| c["text"].as_str().is_some_and(|t| t.contains("Another SolidCraft tab")));
    check(r["isError"] != true && notice, &format!("the agent hears another tab took over ({r})"))?;
    let page = cdp.call("Page.captureScreenshot", json!({"format": "png"}))?;
    std::fs::write(out.join("replaced.png"), unbase64(page["data"].as_str().unwrap_or(""))?).map_err(|e| e.to_string())?;
    println!("web-mcp-test: kept {}", out.join("replaced.png").display());
    println!("web-mcp-test: all checks passed (pictures in {})", out.display());
    Ok(())
}

/// Ask `f` until it says yes (a file loading in the tab takes a few frames), up to the launch timeout.
fn wait_until(mut f: impl FnMut() -> Result<bool, String>) -> Result<bool, String> {
    let t = Instant::now();
    while t.elapsed() < LAUNCH_TIMEOUT {
        if f()? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbase64_reads_what_base64_writes() {
        assert_eq!(unbase64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(unbase64("Zm9vYmFy").unwrap(), b"foobar");
        assert!(unbase64("a!").is_err());
    }
}
