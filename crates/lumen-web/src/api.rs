//! The control plane's API.
//!
//! The server knows how to speak HTTP and how to serve the dashboard. It
//! does not know what a host is: the host implements [`Api`] and the server
//! calls it. That keeps the dependency arrow pointing one way, from the
//! host to the web crate, so the web crate stays free of the pipeline, the
//! encoder and the session table.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::http::{Request, Response, write_event, write_keep_alive, write_response};

/// A command the host refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// A short, stable code the dashboard can switch on.
    pub code: &'static str,
    /// The message to show the person.
    pub message: String,
}

impl ApiError {
    /// A refusal.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error as JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "error": self.code, "message": self.message })
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

/// What the dashboard can ask the host.
pub trait Api: Send + Sync + 'static {
    /// The state of the host, as the dashboard shows it.
    fn state(&self) -> Value;

    /// Apply a command.
    ///
    /// The name is the command, the argument is whatever the dashboard sent.
    /// The result is what the dashboard shows afterwards.
    ///
    /// # Errors
    /// Returns the refusal when the command is unknown or cannot be applied.
    fn command(&self, name: &str, args: &Value) -> Result<Value, ApiError>;

    /// The log lines written after sequence number `after`, oldest first. The lines carry `seq`.
    fn log(&self, after: u64) -> Vec<Value>;
}

/// The routes the server answers.
#[derive(Debug, Clone, Copy)]
pub enum Route<'a> {
    /// The dashboard itself.
    Dashboard,
    /// The host's state.
    State,
    /// The recent log.
    Log,
    /// The event stream.
    Stream,
    /// A command.
    Command,
    /// Nothing.
    NotFound(&'a str),
}

/// Work out which route a request is for.
#[must_use]
pub fn route(request: &Request) -> Route<'_> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/" | "/index.html") => Route::Dashboard,
        ("GET", "/api/state") => Route::State,
        ("GET", "/api/log") => Route::Log,
        ("GET", "/api/stream") => Route::Stream,
        ("POST", "/api/command") => Route::Command,
        (_, path) => Route::NotFound(path),
    }
}

/// The number that `key` holds in a query string such as `after=12&x=1`, if it has one.
fn query_number(query: &str, key: &str) -> Option<u64> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// Whether a request may change the host.
///
/// The server is a local one, and a local server that changes the state of
/// the host is a target: any page in any browser on the machine could post
/// to it. Two things guard it. The mutating requests carry a token that the
/// host printed at start-up, so a page that does not know it cannot act.
/// And a request that names an origin the server does not serve is refused
/// outright, which is the browser's own cross-origin rule.
#[must_use]
pub fn authorises(request: &Request, token: &str, address: &str) -> bool {
    if let Some(origin) = request.header("origin") {
        let allowed = origin == format!("http://{address}")
            || origin == format!("http://localhost:{address}")
            || origin == format!("http://127.0.0.1:{address}");
        if !allowed {
            return false;
        }
    }
    request
        .header("x-lumen-token")
        .is_some_and(|given| given == token)
}

/// Answer a request.
///
/// Returns whether the connection may stay open.
pub fn handle(
    request: &Request,
    stream: &mut std::net::TcpStream,
    api: &Arc<dyn Api>,
    token: &str,
    address: &str,
    dashboard: &str,
) -> std::io::Result<bool> {
    match route(request) {
        Route::Dashboard => {
            let response = Response::with_body(
                200,
                "OK",
                "text/html; charset=utf-8",
                dashboard.as_bytes().to_vec(),
            );
            write_response(stream, &response, true)?;
            Ok(true)
        }
        Route::State => {
            let response = Response::json(&api.state());
            write_response(stream, &response, true)?;
            Ok(true)
        }
        Route::Log => {
            let after = query_number(&request.query, "after").unwrap_or(0);
            let response = Response::json(&Value::Array(api.log(after)));
            write_response(stream, &response, true)?;
            Ok(true)
        }
        Route::Stream => {
            // The event stream owns the connection until the client goes.
            stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
            let header = "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: keep-alive\r\n\r\n"
                .to_string();
            use std::io::Write;
            stream.write_all(header.as_bytes())?;
            stream.flush()?;
            loop {
                // The log is left out of the stream: it changes rarely, and
                // sending it twice a second would be most of the traffic.
                // The dashboard asks for it on its own.
                let mut state = api.state();
                if let Value::Object(map) = &mut state {
                    map.remove("log");
                }
                write_event(stream, "state", &state)?;
                // The dashboard draws a chart from the stream, so the state
                // has to arrive often enough for the chart to fill. The
                // keep-alive comment goes out with it, so a quiet link does
                // not time the connection out.
                std::thread::sleep(crate::http::PUSH_INTERVAL);
                write_keep_alive(stream)?;
            }
        }
        Route::Command => {
            if !authorises(request, token, address) {
                let response = Response::json_status(
                    403,
                    "Forbidden",
                    &ApiError::new(
                        "forbidden",
                        "the request does not carry the host's token, or it comes from another origin",
                    )
                    .to_json(),
                );
                write_response(stream, &response, false)?;
                return Ok(false);
            }
            let body: Value = if request.body.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(e) => {
                        let response = Response::json_status(
                            400,
                            "Bad Request",
                            &ApiError::new("bad_request", format!("the body is not JSON: {e}"))
                                .to_json(),
                        );
                        write_response(stream, &response, false)?;
                        return Ok(false);
                    }
                }
            };
            let name = body
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = body.get("args").cloned().unwrap_or(Value::Null);
            let response = match api.command(&name, &args) {
                Ok(value) => Response::json(&value),
                Err(e) => Response::json_status(400, "Bad Request", &e.to_json()),
            };
            write_response(stream, &response, true)?;
            Ok(true)
        }
        Route::NotFound(path) => {
            let response = Response::json_status(
                404,
                "Not Found",
                &ApiError::new("not_found", format!("there is no {path}")).to_json(),
            );
            write_response(stream, &response, false)?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<HashMap<_, _>>(),
            body: Vec::new(),
        }
    }

    #[test]
    fn the_routes_are_the_ones_the_dashboard_uses() {
        let dashboard = request("GET", "/", &[]);
        assert!(matches!(route(&dashboard), Route::Dashboard));
        let state = request("GET", "/api/state", &[]);
        assert!(matches!(route(&state), Route::State));
        let log = request("GET", "/api/log", &[]);
        assert!(matches!(route(&log), Route::Log));
        let stream = request("GET", "/api/stream", &[]);
        assert!(matches!(route(&stream), Route::Stream));
        let command = request("POST", "/api/command", &[]);
        assert!(matches!(route(&command), Route::Command));
        let missing = request("GET", "/nope", &[]);
        assert!(matches!(route(&missing), Route::NotFound("/nope")));
    }

    #[test]
    fn a_command_needs_the_token() {
        let address = "127.0.0.1:8080";
        let bare = request("POST", "/api/command", &[]);
        assert!(!authorises(&bare, "secret", address));
        let with_token = request("POST", "/api/command", &[("x-lumen-token", "secret")]);
        assert!(authorises(&with_token, "secret", address));
    }

    #[test]
    fn a_command_from_another_origin_is_refused() {
        let address = "127.0.0.1:8080";
        let refused = request(
            "POST",
            "/api/command",
            &[
                ("x-lumen-token", "secret"),
                ("origin", "http://evil.example"),
            ],
        );
        assert!(!authorises(&refused, "secret", address));
        let allowed = request(
            "POST",
            "/api/command",
            &[
                ("x-lumen-token", "secret"),
                ("origin", "http://127.0.0.1:8080"),
            ],
        );
        assert!(authorises(&allowed, "secret", address));
    }
}
