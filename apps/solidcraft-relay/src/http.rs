//! Just enough HTTP/1.1 for the relay: request heads (parsed by `httparse`), bodies with a
//! length or in chunks, keep-alive and `Expect: 100-continue`, and plain responses. Everything a
//! client sends is hostile: heads, bodies and chunk sizes have caps, and a broken request ends its
//! connection with a status, never a panic.

use std::io::{BufRead, Read, Write};

/// Longest request head (request line and headers).
pub const MAX_HEAD: usize = 16 * 1024;
/// Most headers in one request.
const MAX_HEADERS: usize = 64;

pub struct Request {
    pub method: String,
    /// The path without its query.
    pub path: String,
    /// Header names in lower case, in the order sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The client keeps the connection open for another request.
    pub keep_alive: bool,
}

impl Request {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Whether the header `name` lists `token` (comma-separated, any case), as `Connection: keep-alive, Upgrade`.
    pub fn header_has(&self, name: &str, token: &str) -> bool {
        self.headers.iter().filter(|(n, _)| n == name).any(|(_, v)| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }
}

/// Why a request could not be read: answer with this status and close the connection.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub message: &'static str,
}

const fn refuse(status: u16, message: &'static str) -> Refusal {
    Refusal { status, message }
}

/// The end of reading a request.
pub enum Incoming {
    Request(Request),
    /// The client closed the connection (or went quiet) between requests.
    Closed,
    Refused(Refusal),
}

/// Read one request from `reader`, with a body of at most `max_body` bytes. `out` gets the
/// interim `100 Continue` a client may wait for before it sends the body.
pub fn read_request(reader: &mut impl BufRead, out: &mut impl Write, max_body: usize) -> Incoming {
    let head = match read_head(reader) {
        Ok(Some(head)) => head,
        Ok(None) => return Incoming::Closed,
        Err(r) => return Incoming::Refused(r),
    };
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Incoming::Refused(refuse(400, "incomplete request head")),
        Err(httparse::Error::TooManyHeaders) => return Incoming::Refused(refuse(431, "too many headers")),
        Err(_) => return Incoming::Refused(refuse(400, "malformed request")),
    }
    let (Some(method), Some(target), Some(minor)) = (parsed.method, parsed.path, parsed.version) else {
        return Incoming::Refused(refuse(400, "malformed request"));
    };
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> =
        parsed.headers.iter().map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).trim().to_string())).collect();
    let mut req = Request { method: method.to_string(), path, headers, body: Vec::new(), keep_alive: false };
    // HTTP/1.1 keeps the connection unless told otherwise; 1.0 only when asked.
    req.keep_alive = if minor >= 1 { !req.header_has("connection", "close") } else { req.header_has("connection", "keep-alive") };
    let chunked = req.header_has("transfer-encoding", "chunked");
    let length = match req.header("content-length").map(|l| l.parse::<usize>()) {
        None => None,
        Some(Ok(n)) => Some(n),
        Some(Err(_)) => return Incoming::Refused(refuse(400, "bad Content-Length")),
    };
    if chunked && length.is_some() {
        return Incoming::Refused(refuse(400, "both Content-Length and chunked"));
    }
    if req.header("transfer-encoding").is_some() && !chunked {
        return Incoming::Refused(refuse(501, "unsupported Transfer-Encoding"));
    }
    if length.is_some_and(|n| n > max_body) {
        return Incoming::Refused(refuse(413, "request body too large"));
    }
    let has_body = chunked || length.is_some_and(|n| n > 0);
    if has_body && req.header_has("expect", "100-continue") && out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").and_then(|()| out.flush()).is_err() {
        return Incoming::Closed;
    }
    let body = if chunked { read_chunked(reader, max_body) } else { read_exact(reader, length.unwrap_or(0)) };
    match body {
        Ok(body) => req.body = body,
        Err(r) => return Incoming::Refused(r),
    }
    Incoming::Request(req)
}

/// The head up to and with its blank line; `None` at the end of the stream before any byte.
fn read_head(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, Refusal> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = match reader.by_ref().take((MAX_HEAD - head.len() + 1) as u64).read_until(b'\n', &mut line) {
            Ok(n) => n,
            // Idle between requests (a kept-alive connection the client no longer uses): closed.
            Err(_) if head.is_empty() && line.is_empty() => return Ok(None),
            Err(_) => return Err(refuse(408, "request head not received")),
        };
        if n == 0 {
            return if head.is_empty() { Ok(None) } else { Err(refuse(400, "incomplete request head")) };
        }
        // Empty lines before a request are allowed (RFC 9112 §2.2).
        if head.is_empty() && (line == b"\r\n" || line == b"\n") {
            continue;
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD {
            return Err(refuse(431, "request head too large"));
        }
        if !line.ends_with(b"\n") {
            return Err(refuse(400, "incomplete request head"));
        }
        if line == b"\r\n" || line == b"\n" {
            return Ok(Some(head));
        }
    }
}

fn read_exact(reader: &mut impl BufRead, n: usize) -> Result<Vec<u8>, Refusal> {
    let mut body = Vec::with_capacity(n.min(1 << 20));
    let got = reader.by_ref().take(n as u64).read_to_end(&mut body).map_err(|_| refuse(408, "request body not received"))?;
    if got < n {
        return Err(refuse(400, "request body shorter than its Content-Length"));
    }
    Ok(body)
}

/// A chunked body (RFC 9112 §7.1); chunk extensions and trailers are read and dropped.
fn read_chunked(reader: &mut impl BufRead, max: usize) -> Result<Vec<u8>, Refusal> {
    let mut body = Vec::new();
    loop {
        let mut line = Vec::new();
        reader.by_ref().take(1024).read_until(b'\n', &mut line).map_err(|_| refuse(408, "request body not received"))?;
        let text = std::str::from_utf8(&line).map_err(|_| refuse(400, "bad chunk size"))?;
        let size = text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size, 16).map_err(|_| refuse(400, "bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line.
            for _ in 0..MAX_HEADERS {
                let mut t = Vec::new();
                let n = reader.by_ref().take(MAX_HEAD as u64).read_until(b'\n', &mut t).map_err(|_| refuse(408, "request body not received"))?;
                if n == 0 || t == b"\r\n" || t == b"\n" {
                    return Ok(body);
                }
            }
            return Err(refuse(431, "too many trailers"));
        }
        if body.len().saturating_add(size) > max {
            return Err(refuse(413, "request body too large"));
        }
        let chunk = read_exact(reader, size)?;
        body.extend_from_slice(&chunk);
        let mut end = [0u8; 2];
        reader.read_exact(&mut end).map_err(|_| refuse(400, "chunk without its line end"))?;
        if &end != b"\r\n" {
            return Err(refuse(400, "chunk without its line end"));
        }
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    /// Close the connection after this response.
    pub close: bool,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new(), close: false }
    }
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::new(status).header("Content-Type", "application/json").body(body.to_string().into_bytes())
    }
    pub fn text(status: u16, text: &str) -> Self {
        Self::new(status).header("Content-Type", "text/plain; charset=utf-8").body(format!("{text}\n").into_bytes())
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
    pub fn closing(mut self) -> Self {
        self.close = true;
        self
    }

    /// Write a `101 Switching Protocols` head with exactly its own headers: the connection is then
    /// another protocol's.
    pub fn write_upgrade(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            let value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        out.write_all(head.as_bytes())?;
        out.flush()
    }

    /// Write it, with `Content-Length` and `Connection`.
    pub fn write(&self, out: &mut impl Write, keep_alive: bool) -> std::io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            // Values are ours; a line break would end the head early, so none gets through.
            let value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        head.push_str("Cache-Control: no-store\r\n");
        head.push_str(if keep_alive && !self.close { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
        out.write_all(head.as_bytes())?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8]) -> (Incoming, Vec<u8>) {
        let mut out = Vec::new();
        let r = read_request(&mut std::io::BufReader::new(raw), &mut out, 64);
        (r, out)
    }

    fn request(raw: &[u8]) -> Request {
        match read(raw).0 {
            Incoming::Request(r) => r,
            Incoming::Closed => panic!("closed"),
            Incoming::Refused(r) => panic!("refused: {r:?}"),
        }
    }

    fn refused(raw: &[u8]) -> u16 {
        match read(raw).0 {
            Incoming::Refused(r) => r.status,
            _ => panic!("not refused: {}", String::from_utf8_lossy(raw)),
        }
    }

    #[test]
    fn requests_with_a_length_or_in_chunks() {
        let r = request(b"POST /mcp?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nAuthorization: Bearer t\r\n\r\nhello");
        assert_eq!((r.method.as_str(), r.path.as_str(), r.body.as_slice(), r.keep_alive), ("POST", "/mcp", &b"hello"[..], true));
        assert_eq!(r.header("authorization"), Some("Bearer t"));
        let r = request(
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3;x=y\r\nhel\r\n2\r\nlo\r\n0\r\nTrailer: 1\r\n\r\n",
        );
        assert_eq!((r.body.as_slice(), r.keep_alive), (&b"hello"[..], false));
        let r = request(b"\r\nGET /healthz HTTP/1.0\r\n\r\n");
        assert_eq!((r.path.as_str(), r.keep_alive, r.body.len()), ("/healthz", false, 0));
        assert!(matches!(read(b"").0, Incoming::Closed));
    }

    #[test]
    fn a_waiting_client_is_told_to_continue() {
        let (r, out) = read(b"POST /mcp HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n{}");
        assert!(matches!(r, Incoming::Request(_)));
        assert_eq!(out, b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    #[test]
    fn hostile_requests_are_refused() {
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nContent-Length: 65\r\n\r\n"), 413);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n"), 400);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort"), 400);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffffffff\r\n"), 400);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n41\r\n"), 413);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXY"), 400);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n"), 501);
        assert_eq!(refused(b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n"), 400);
        assert_eq!(refused(b"NOT A REQUEST\r\n\r\n"), 400);
        assert_eq!(refused(b"GET / HTTP/1.1\r\nHost: a"), 400);
        let mut huge = b"GET / HTTP/1.1\r\n".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_HEAD + 10));
        assert_eq!(refused(&huge), 431);
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..100 {
            many.extend(format!("X-{i}: 1\r\n").as_bytes());
        }
        many.extend(b"\r\n");
        assert_eq!(refused(&many), 431);
    }

    #[test]
    fn responses_carry_their_length_and_no_injected_lines() {
        let mut out = Vec::new();
        Response::text(404, "nothing here").header("X-Note", "a\r\nInjected: yes").write(&mut out, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n") && text.contains("Content-Length: 13\r\n"), "{text}");
        assert!(text.contains("X-Note: aInjected: yes\r\n") && text.contains("Connection: keep-alive"), "{text}");
        let mut out = Vec::new();
        Response::new(202).closing().write(&mut out, true).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Connection: close"));
    }
}
