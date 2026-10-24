//! The stdio transport: newline-delimited JSON-RPC on stdin and stdout (`solidcraft-cli mcp`).
//! Each line goes to the transport-independent [`Server::handle_line`].

use std::io::{BufRead, Write};

use serde_json::Value;
/// Longest accepted request line, shared with the control server.
use solidcraft_engine::MAX_LINE;

use crate::server::{INVALID_REQUEST, PARSE_ERROR, Server, error};

impl Server {
    /// Serve newline-delimited JSON-RPC until `input` closes. Only protocol messages go to
    /// `output`; diagnostics belong on stderr. Consume oversized lines in bounded space so an
    /// agent cannot exhaust memory by sending an arbitrarily long request without a newline.
    pub fn serve(&mut self, mut input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        loop {
            let mut line = Vec::new();
            let mut oversized = false;
            let mut received = false;
            loop {
                let chunk = input.fill_buf()?;
                if chunk.is_empty() {
                    break;
                }
                let count = chunk.iter().position(|&byte| byte == b'\n').map_or(chunk.len(), |i| i + 1);
                let newline = chunk[count - 1] == b'\n';
                let content_len = count - usize::from(newline);
                if !oversized {
                    if line.len().saturating_add(content_len) > MAX_LINE {
                        oversized = true;
                        line.clear();
                    } else {
                        line.extend_from_slice(&chunk[..content_len]);
                    }
                }
                input.consume(count);
                received = true;
                if newline {
                    break;
                }
            }
            if !received {
                break;
            }
            let reply = if oversized {
                Some(error(Value::Null, INVALID_REQUEST, "request too large").to_string())
            } else {
                match String::from_utf8(line) {
                    Ok(line) => self.handle_line(&line),
                    // Not UTF-8: answer with a parse error and keep serving.
                    Err(_) => Some(error(Value::Null, PARSE_ERROR, "parse error: input is not UTF-8").to_string()),
                }
            };
            if let Some(reply) = reply {
                output.write_all(reply.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}
