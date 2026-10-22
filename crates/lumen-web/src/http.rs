//! A minimal HTTP/1.1 server.
//!
//! The workspace has no web framework, and adding one would pull a large
//! dependency tree into a project whose dependency policy is "permissive
//! licences only, no exceptions". A control plane that serves one page and
//! a few JSON documents does not need a framework: it needs a socket, a
//! request parser and a response writer.
//!
//! What is supported:
//!
//! - HTTP/1.1 requests with a URL, a method, headers and an optional body
//!   sized by `Content-Length`;
//! - keep-alive, which is the default of HTTP/1.1 and what a browser wants;
//! - server-sent events, for a dashboard that updates itself;
//! - a read timeout, so a client that stops talking does not hold a thread.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde_json::Value;

/// The largest request body the server accepts.
const MAX_BODY: usize = 1 << 20;

/// The largest request head the server accepts.
const MAX_HEAD: usize = 16 * 1024;

/// How long a connection may stay idle before the server gives up on it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a server-sent event connection may stay idle.
const STREAM_TIMEOUT: Duration = Duration::from_secs(15);

/// A request the server has read.
#[derive(Debug, Clone)]
pub struct Request {
    /// The method, upper case.
    pub method: String,
    /// The path, without the query string.
    pub path: String,
    /// The query string, without the leading question mark.
    pub query: String,
    /// The headers, with the names lower case.
    pub headers: HashMap<String, String>,
    /// The body.
    pub body: Vec<u8>,
}

impl Request {
    /// The value of a header, if it is there.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A response the server writes.
#[derive(Debug, Clone)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The reason phrase.
    pub reason: &'static str,
    /// The headers.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A response with a body and a content type.
    #[must_use]
    pub fn with_body(status: u16, reason: &'static str, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
            body,
        }
    }

    /// A JSON response.
    #[must_use]
    pub fn json(value: &Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::with_body(200, "OK", "application/json; charset=utf-8", body)
    }

    /// A JSON response with a status code.
    #[must_use]
    pub fn json_status(status: u16, reason: &'static str, value: &Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::with_body(status, reason, "application/json; charset=utf-8", body)
    }

    /// A plain text response.
    #[must_use]
    pub fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Self::with_body(
            status,
            reason,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    /// The header block and the body, ready to be written.
    #[must_use]
    pub fn to_bytes(&self, keep_alive: bool) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            let _ = write!(out, "{name}: {value}\r\n");
        }
        let _ = write!(out, "Content-Length: {}\r\n", self.body.len());
        out.push_str(if keep_alive {
            "Connection: keep-alive\r\n"
        } else {
            "Connection: close\r\n"
        });
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Read one request from the stream.
///
/// Returns `Ok(None)` when the peer closed the connection between requests,
/// which is the normal end of a keep-alive conversation.
pub fn read_request(stream: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    let read = stream.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    // A client may send a request with leading blank lines; skip them.
    while line.trim().is_empty() {
        line.clear();
        let read = stream.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
    }
    if line.len() > MAX_HEAD {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "the request line is too long",
        ));
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut headers = HashMap::new();
    let mut head_len = line.len();
    loop {
        let mut header_line = String::new();
        let read = stream.read_line(&mut header_line)?;
        if read == 0 {
            break;
        }
        head_len += read;
        if head_len > MAX_HEAD {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "the request head is too long",
            ));
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "the request body is too large",
        ));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

/// Write a response to the stream.
pub fn write_response(
    stream: &mut TcpStream,
    response: &Response,
    keep_alive: bool,
) -> std::io::Result<()> {
    stream.write_all(&response.to_bytes(keep_alive))?;
    stream.flush()
}

/// The loop that serves one connection.
pub fn serve_connection<F>(mut stream: TcpStream, handler: F) -> std::io::Result<()>
where
    F: Fn(Request, &mut TcpStream) -> std::io::Result<bool>,
{
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match handler(request, &mut stream) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
}

/// The listener the server accepts on.
#[derive(Debug)]
pub struct Listener {
    /// The bound listener.
    pub listener: TcpListener,
    /// The address it is bound to.
    pub address: SocketAddr,
}

impl Listener {
    /// Bind the listener.
    ///
    /// # Errors
    /// Returns the error the operating system reported.
    pub fn bind(address: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        let address = listener.local_addr()?;
        Ok(Self { listener, address })
    }
}

/// Write one server-sent event.
///
/// # Errors
/// Returns the error the operating system reported.
pub fn write_event(stream: &mut TcpStream, event: &str, data: &Value) -> std::io::Result<()> {
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    let mut frame = format!("event: {event}\ndata: {payload}\n\n");
    if event == "state" {
        frame.push_str(": keep-alive\n\n");
    }
    stream.write_all(frame.as_bytes())?;
    stream.flush()
}

/// The keep-alive comment that keeps an event stream open.
pub fn write_keep_alive(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b": keep-alive\n\n")?;
    stream.flush()
}

/// How long an event stream waits before it sends the next state.
pub const PUSH_INTERVAL: Duration = Duration::from_millis(500);

/// How long an event stream waits before it sends a keep-alive on its own.
pub const KEEP_ALIVE: Duration = STREAM_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_response_carries_its_length_and_type() {
        let response = Response::json(&serde_json::json!({"a": 1}));
        let bytes = response.to_bytes(false);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json; charset=utf-8\r\n"));
        assert!(text.contains(&format!("Content-Length: {}\r\n", response.body.len())));
        assert!(text.contains("Connection: close\r\n\r\n"));
        // The head, then the body.
        let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body, "{\"a\":1}");
    }

    #[test]
    fn the_error_response_has_the_right_status() {
        let response = Response::json_status(403, "Forbidden", &serde_json::json!({"error": "no"}));
        let bytes = response.to_bytes(false);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }
}
