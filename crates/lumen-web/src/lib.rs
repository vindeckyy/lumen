//! Lumen's control plane: a small HTTP server and the dashboard it serves.
//!
//! The host runs the server on a loopback address and prints the URL, with
//! a token in the fragment, so the dashboard can be opened in a browser on
//! the same machine. Everything the dashboard shows comes from the host's
//! own state, and everything it can change goes through the host's own
//! command handler.
//!
//! The crate has no framework behind it on purpose. A control plane serves
//! one page, one state document and one command endpoint; the HTTP that
//! takes is small enough to own, and owning it keeps the dependency tree
//! of the project small as well.

#![forbid(unsafe_code)]

pub mod api;
pub mod http;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

pub use api::{Api, ApiError, Route};

/// The number of random bytes in the token.
const TOKEN_BYTES: usize = 32;

/// The control plane.
pub struct WebServer {
    /// The listener.
    listener: http::Listener,
    /// The host behind it.
    api: Arc<dyn Api>,
    /// The token a mutating request has to carry.
    token: String,
}

impl std::fmt::Debug for WebServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServer")
            .field("address", &self.listener.address)
            .finish_non_exhaustive()
    }
}

impl WebServer {
    /// Bind the control plane.
    ///
    /// # Errors
    /// Returns the error the operating system reported.
    pub fn bind(address: SocketAddr, api: Arc<dyn Api>) -> std::io::Result<Self> {
        let listener = http::Listener::bind(address)?;
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::fill(&mut bytes);
        let token = bytes.iter().fold(String::new(), |mut acc, byte| {
            use std::fmt::Write;
            let _ = write!(acc, "{byte:02x}");
            acc
        });
        Ok(Self {
            listener,
            api,
            token,
        })
    }

    /// The address the control plane listens on.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.listener.address
    }

    /// The token a mutating request has to carry.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The URL to open in a browser.
    ///
    /// The token is in the fragment, so it never reaches the server as part
    /// of the path and never lands in a proxy's log.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/#{}", self.listener.address, self.token)
    }

    /// Serve until the process ends.
    ///
    /// # Errors
    /// Returns the error the operating system reported.
    pub fn run(self) -> std::io::Result<()> {
        let address = self.listener.address.to_string();
        let api = Arc::clone(&self.api);
        let token = self.token.clone();
        let dashboard = ui::DASHBOARD.to_string();
        for stream in self.listener.listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("lumen-web: cannot accept: {e}");
                    continue;
                }
            };
            let api = Arc::clone(&api);
            let token = token.clone();
            let dashboard = dashboard.clone();
            let address = address.clone();
            std::thread::spawn(move || {
                if let Err(e) = http::serve_connection(stream, |request, out| {
                    api::handle(&request, out, &api, &token, &address, &dashboard)
                }) {
                    eprintln!("lumen-web: the connection failed: {e}");
                }
            });
        }
        Ok(())
    }

    /// Serve on a thread of its own.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            if let Err(e) = self.run() {
                eprintln!("lumen-web: the control plane stopped: {e}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A loopback address with a port the operating system picks.
    const LOOPBACK: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

    struct Empty;

    impl Api for Empty {
        fn state(&self) -> Value {
            json!({ "ok": true })
        }

        fn command(&self, name: &str, _args: &Value) -> Result<Value, ApiError> {
            if name == "ping" {
                Ok(json!({ "pong": true }))
            } else {
                Err(ApiError::new(
                    "unknown_command",
                    format!("there is no {name}"),
                ))
            }
        }

        fn log(&self) -> Vec<Value> {
            vec![json!({ "level": "info", "message": "hello" })]
        }
    }

    #[test]
    fn the_token_is_long_and_different_every_time() {
        let a = WebServer::bind(LOOPBACK, Arc::new(Empty)).expect("bind");
        let b = WebServer::bind(LOOPBACK, Arc::new(Empty)).expect("bind");
        assert_eq!(a.token().len(), TOKEN_BYTES * 2);
        assert_ne!(a.token(), b.token());
    }

    #[test]
    fn the_url_carries_the_token_in_the_fragment() {
        let server = WebServer::bind(LOOPBACK, Arc::new(Empty)).expect("bind");
        let url = server.url();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.ends_with(&format!("#{}", server.token())));
    }
}
