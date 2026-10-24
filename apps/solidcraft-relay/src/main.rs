//! `solidcraft-relay`: SolidCraft's MCP server for the web app (docs/web-mcp.md).
//!
//! An agent connects to `POST /mcp` (MCP over Streamable HTTP, `Authorization: Bearer TOKEN`);
//! the SolidCraft web app in a browser tab connects to `/bridge` (a WebSocket, the same token as
//! its first message). The agent's tool calls are the desktop MCP server's (`crates/mcp`); they
//! become control requests the tab runs on the design open in it. Optionally, the relay keeps a
//! headless session of its own for when no tab is connected (`SOLIDCRAFT_RELAY_HEADLESS`).
//!
//! Settings come from the environment: `SOLIDCRAFT_RELAY_TOKEN` (required) or
//! `SOLIDCRAFT_RELAY_TOKEN_FILE`, `SOLIDCRAFT_RELAY_LISTEN` (default 127.0.0.1:8090),
//! `SOLIDCRAFT_RELAY_ALLOWED_ORIGIN`, `SOLIDCRAFT_RELAY_PUBLIC_URL`, `SOLIDCRAFT_RELAY_HEADLESS`
//! (off, fallback or only) and `SOLIDCRAFT_RELAY_DATA`. Put it behind a TLS reverse proxy, on the
//! same site as the web app (packaging/docker).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod bridge;
mod config;
mod http;
mod mcp;
#[cfg(test)]
mod tests;

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use solidcraft_mcp::{HeadlessMode, SharedHeadless};

use crate::config::Config;
use crate::http::{Incoming, Response};

/// Connections at once (agents, tabs and anything else); more are refused.
pub const MAX_CONNECTIONS: usize = 64;
/// How long a connection may wait between requests, or take to send one.
const IDLE: Duration = Duration::from_secs(120);
/// The largest body read for a request other than an agent's.
const SMALL_BODY: usize = 64 * 1024;

/// What a request is for: the last part of its path (`mcp`, `bridge`, `healthz`), so the relay
/// works under a path prefix too.
fn route(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

const USAGE: &str = "solidcraft-relay: SolidCraft's MCP server for the web app (docs/web-mcp.md)

Agents connect to POST /mcp (Streamable HTTP, Authorization: Bearer TOKEN); the web app in a
browser tab connects to /bridge once opened as https://HOST/#mcp=TOKEN.

Options:
  --health    ask the relay listening at SOLIDCRAFT_RELAY_LISTEN whether it runs (for health checks)
  --version   print the version

Environment:
  SOLIDCRAFT_RELAY_TOKEN           the token (required; or SOLIDCRAFT_RELAY_TOKEN_FILE)
  SOLIDCRAFT_RELAY_LISTEN          address to listen on (default 127.0.0.1:8090)
  SOLIDCRAFT_RELAY_ALLOWED_ORIGIN  other sites whose pages may connect a tab (comma-separated)
  SOLIDCRAFT_RELAY_PUBLIC_URL      the web app's address, for hints (default: from each request)
  SOLIDCRAFT_RELAY_HEADLESS        off (default), fallback or only: a headless session on the server
  SOLIDCRAFT_RELAY_DATA            the headless session's folder (default ./solidcraft-relay-data)
";

/// Everything the connections share.
pub struct Relay {
    pub config: Config,
    pub hub: Arc<bridge::Hub>,
    pub sessions: mcp::Sessions,
    pub headless: Option<Arc<SharedHeadless>>,
    open: AtomicUsize,
    /// The bridge's heartbeat (tests shorten it).
    pub ping_every: Duration,
    pub silence: Duration,
}

impl Relay {
    pub fn new(config: Config) -> Result<Self, String> {
        let headless = match config.headless {
            HeadlessMode::Off => None,
            _ => {
                std::fs::create_dir_all(&config.data).map_err(|e| format!("SOLIDCRAFT_RELAY_DATA {}: {e}", config.data.display()))?;
                let folder = std::fs::canonicalize(&config.data).map_err(|e| format!("SOLIDCRAFT_RELAY_DATA {}: {e}", config.data.display()))?;
                install_renderer();
                Some(Arc::new(SharedHeadless::new(folder)))
            }
        };
        Ok(Self {
            config,
            hub: Arc::default(),
            sessions: mcp::Sessions::default(),
            headless,
            open: AtomicUsize::new(0),
            ping_every: bridge::PING_EVERY,
            silence: bridge::SILENCE,
        })
    }
}

/// The headless session's pictures: the CPU rasterizer (a server has no GPU to count on).
fn install_renderer() {
    solidcraft_engine::capture::install(Arc::new(|scene, cam, w, h| {
        let canvas = solidcraft_engine::render::render(scene, cam, w as usize, h as usize);
        Ok(solidcraft_engine::capture::Picture { rgba: canvas.rgba, renderer: "cpu" })
    }));
}

/// Serve `listener` until it fails.
pub fn serve(listener: TcpListener, relay: Arc<Relay>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if relay.open.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            relay.open.fetch_sub(1, Ordering::Relaxed);
            let mut s = stream;
            let _ = Response::text(503, "too many connections").closing().write(&mut s, false);
            continue;
        }
        let relay2 = relay.clone();
        let spawned = std::thread::Builder::new().name("relay-connection".into()).spawn(move || {
            connection(&relay2, stream);
            relay2.open.fetch_sub(1, Ordering::Relaxed);
        });
        if spawned.is_err() {
            relay.open.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn connection(relay: &Relay, stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_write_timeout(Some(IDLE));
    let Ok(read) = stream.try_clone() else { return };
    let mut out = stream;
    let mut reader = BufReader::with_capacity(64 * 1024, read);
    loop {
        // A body is read only for a request that may have one: an agent's (with the token), up to
        // the control channel's line; anything else is answered from its head.
        let limit = |req: &http::Request| match route(&req.path) {
            "mcp" if mcp::authorized(req, &relay.config.token) => Some(solidcraft_engine::MAX_LINE),
            "mcp" => None,
            _ => Some(SMALL_BODY),
        };
        let req = match http::read_request(&mut reader, &mut out, limit) {
            Incoming::Request(r) => r,
            Incoming::Closed => return,
            Incoming::Refused(r) => {
                let _ = Response::text(r.status, r.message).closing().write(&mut out, false);
                return;
            }
        };
        let response = match route(&req.path) {
            "mcp" => mcp::handle(relay, &req),
            "bridge" => match bridge::serve(relay, &req, reader, &peer) {
                Some(refusal) => {
                    let _ = refusal.closing().write(&mut out, false);
                    return;
                }
                None => return,
            },
            "healthz" if req.method == "GET" => Response::text(200, "ok"),
            _ => Response::text(404, "not found: the relay serves /mcp (agents) and /bridge (the web app)"),
        };
        if response.status == 401 {
            eprintln!("solidcraft-relay: refused {} {} from {peer}: no or wrong token", req.method, req.path);
        }
        // A body left unread is still on the connection: it can't carry another request.
        if response.write(&mut out, req.keep_alive && !req.unread_body).is_err() || !req.keep_alive || req.unread_body || response.close {
            return;
        }
    }
}

/// `--health`: whether the relay at `SOLIDCRAFT_RELAY_LISTEN` answers `/healthz` (a container's
/// health check: the relay's image has no shell or HTTP client).
fn health() -> std::process::ExitCode {
    use std::io::{Read as _, Write as _};
    let listen = std::env::var("SOLIDCRAFT_RELAY_LISTEN").ok().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| config::DEFAULT_LISTEN.into());
    let Ok(mut addr) = listen.trim().parse::<std::net::SocketAddr>() else {
        eprintln!("solidcraft-relay: SOLIDCRAFT_RELAY_LISTEN `{listen}` is not an address");
        return std::process::ExitCode::FAILURE;
    };
    // Listening everywhere: ask on this computer.
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let answer = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).and_then(|mut s| {
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut reply = Vec::new();
        s.take(4096).read_to_end(&mut reply)?;
        Ok(reply)
    });
    match answer {
        Ok(reply) if reply.starts_with(b"HTTP/1.1 200") => std::process::ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("solidcraft-relay: {addr} answered, but not as a healthy relay");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("solidcraft-relay: nothing answers at {addr}: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("solidcraft-relay {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--health") {
        return health();
    }
    if let Some(a) = args.first() {
        eprintln!("solidcraft-relay: unknown argument `{a}`; settings come from the environment (--help)");
        return std::process::ExitCode::from(2);
    }
    let config = match Config::from_env(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("solidcraft-relay: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let listen = config.listen;
    let relay = match Relay::new(config) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("solidcraft-relay: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let listener = match TcpListener::bind(listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("solidcraft-relay: cannot listen on {listen}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mode = match relay.config.headless {
        HeadlessMode::Off => "no headless session".to_string(),
        HeadlessMode::Fallback => format!(
            "a headless session (files in {}) while no tab is connected",
            relay.headless.as_ref().map(|h| h.folder().display().to_string()).unwrap_or_default()
        ),
        HeadlessMode::Only => {
            format!("a headless session only (files in {})", relay.headless.as_ref().map(|h| h.folder().display().to_string()).unwrap_or_default())
        }
    };
    eprintln!("solidcraft-relay {}: listening on {listen} (POST /mcp for agents, /bridge for the web app; {mode})", env!("CARGO_PKG_VERSION"));
    serve(listener, relay);
    std::process::ExitCode::FAILURE
}
