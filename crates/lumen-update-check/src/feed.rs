//! Fetching the manifest.
//!
//! The manifest and its signature come from the same place: the body is the manifest and the
//! signature is the first 64 bytes of the `X-Lumen-Signature` header, hex encoded. The
//! transport is plain HTTP, because the signature is what makes the answer trustworthy.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::Error;

/// How long the fetch waits.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many redirects are followed.
const MAX_REDIRECTS: usize = 4;

/// The header the signature travels in.
pub const SIGNATURE_HEADER: &str = "x-lumen-signature";

/// Fetches the manifest and its signature.
///
/// # Errors
/// Returns an error when the host cannot be reached, the answer is not a plain HTTP answer, or
/// the signature header is missing or malformed.
pub fn fetch_signed(url: &str) -> Result<(Vec<u8>, [u8; 64]), Error> {
    let mut current = url.to_owned();
    for _ in 0..MAX_REDIRECTS {
        let (body, headers) = fetch(&current)?;
        if let Some(location) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .map(|(_, value)| value.clone())
        {
            current = location;
            continue;
        }
        let signature = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(SIGNATURE_HEADER))
            .map(|(_, value)| value.clone())
            .ok_or_else(|| Error::Fetch("the answer has no signature".into()))?;
        let bytes = hex_decode(signature.trim())
            .ok_or_else(|| Error::Fetch("the signature is not hex".into()))?;
        let signature: [u8; 64] = bytes
            .try_into()
            .map_err(|_| Error::Fetch("the signature is not 64 bytes".into()))?;
        return Ok((body, signature));
    }
    Err(Error::Fetch("too many redirects".into()))
}

/// The headers of an answer.
type Headers = Vec<(String, String)>;

/// One HTTP GET, with the body and the headers.
fn fetch(url: &str) -> Result<(Vec<u8>, Headers), Error> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::Fetch(format!("{url} is not an http url")))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let address = authority
        .rsplit_once(':')
        .filter(|(host, port)| !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map_or_else(
            || (authority.to_owned(), 80u16),
            |(host, port)| (host.to_owned(), port.parse().unwrap_or(80)),
        );
    let mut stream = TcpStream::connect((address.0.as_str(), address.1))
        .map_err(|e| Error::Fetch(format!("cannot reach {authority}: {e}")))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| Error::Fetch(e.to_string()))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| Error::Fetch(e.to_string()))?;
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .map_err(|e| Error::Fetch(e.to_string()))?;
    parse_answer(&answer)
}

/// Splits an HTTP answer into its body and its headers.
fn parse_answer(answer: &[u8]) -> Result<(Vec<u8>, Headers), Error> {
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| Error::Fetch("the answer has no header end".into()))?;
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let body = answer[split + 4..].to_vec();
    let mut lines = head.lines();
    let status = lines
        .next()
        .ok_or_else(|| Error::Fetch("the answer has no status line".into()))?;
    if !status.contains(" 200 ") {
        return Err(Error::Fetch(format!("the answer is {status}")));
    }
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Ok((body, headers))
}

/// The bytes of a hex text.
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_split_into_headers_and_body() {
        let answer = b"HTTP/1.1 200 OK\r\nx-lumen-signature: abcd\r\n\r\nbody";
        let (body, headers) = parse_answer(answer).expect("parses");
        assert_eq!(body, b"body");
        assert_eq!(headers[0].0, "x-lumen-signature");
    }

    #[test]
    fn an_error_status_is_refused() {
        let answer = b"HTTP/1.1 404 Not Found\r\n\r\n";
        assert!(parse_answer(answer).is_err());
    }

    #[test]
    fn hex_is_decoded() {
        assert_eq!(hex_decode("00ff"), Some(vec![0, 255]));
        assert_eq!(hex_decode("0f0"), None);
        assert_eq!(hex_decode("zz"), None);
    }
}
