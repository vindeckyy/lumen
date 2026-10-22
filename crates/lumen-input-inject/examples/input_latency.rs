//! Software input latency: client stamp to host injection, measured across the netns pair.
//!
//! The client stamps each event with `CLOCK_MONOTONIC` just before it is sealed and sent. The host
//! reads the same clock right after the XTEST request is flushed. Both namespaces share one kernel
//! clock, so latency is `injected_us - client_ts_us` with no clock mapping. The result is
//! software-only and provisional. It is not glass-to-glass, and it does not include the display.
//!
//! Setup: `sudo scripts/netns-up.sh`, then an Xvfb at 1920x1080 on DISPLAY `:93`, then
//! `cargo build --release -p lumen-input-inject --example input_latency`. Exact commands:
//!
//! ```text
//! sudo ip netns exec lumen-host env DISPLAY=:93 target/release/examples/input_latency \
//!     host --bind 10.77.0.1:24803 --expect 10000 --label bare-veth \
//!     --out artifacts/m3/input-latency-bare-veth.json
//! sudo ip netns exec lumen-client target/release/examples/input_latency \
//!     client --to 10.77.0.1:24803 --bind 10.77.0.2:0 --count 10000 --rate-hz 1000
//! ```
//!
//! The host must start first. It prints a `listening` line, and the client starts after that. The
//! session key comes from a fixed label. It is a test vector and provides no secrecy.

use std::fmt::Display;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use lumen_crypto::{AeadKind, PacketCipher, SessionKeys};
use lumen_input_inject::{InputReceiver, Outcome, XTestInjector};
use lumen_input_wayland::{InputEvent, InputEventKind, InputSender, now_us};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Session id both roles seal and open with.
const SESSION_ID: u32 = 0x4c4d_4e31;
/// Test-vector label for the client-to-host key. Not a secret.
const KEY_LABEL: &[u8] = b"lumen input-latency harness test vector (not a secret)";
const USAGE: &str = "usage: input_latency <host|client> [--flag value]...";

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let result = match raw.split_first() {
        None => Err(USAGE.to_owned()),
        Some((role, rest)) => Flags::parse(rest).and_then(|flags| match role.as_str() {
            "host" => run_host(&flags),
            "client" => run_client(&flags),
            other => Err(format!("unknown role {other:?}; {USAGE}")),
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("input_latency: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--name value` pairs. A later duplicate overrides an earlier one.
struct Flags(Vec<(String, String)>);

impl Flags {
    fn parse(raw: &[String]) -> Result<Self, String> {
        let mut pairs = Vec::new();
        let mut it = raw.iter();
        while let Some(name) = it.next() {
            let key = name
                .strip_prefix("--")
                .ok_or_else(|| format!("expected --flag, got {name:?}"))?;
            let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
            pairs.push((key.to_owned(), value.clone()));
        }
        Ok(Self(pairs))
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn parsed<T>(&self, key: &str, default: T) -> Result<T, String>
    where
        T: FromStr,
        T::Err: Display,
    {
        match self.value(key) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|e| format!("--{key}: {e}")),
        }
    }
}

fn session_cipher() -> PacketCipher {
    let key = SessionKeys::derive(KEY_LABEL).media_c2h;
    PacketCipher::new(AeadKind::preferred(), key, SESSION_ID)
}

/// Client-surface position of event `i`. A fixed walk over the surface, so the final position is
/// known in advance.
fn position(i: u64, width: u32, height: u32) -> (f64, f64) {
    let x = i * 37 % u64::from(width);
    let y = i * 11 % u64::from(height);
    (x as f64, y as f64)
}

/// Maps a client coordinate to the host screen, the same way the XTEST injector does.
fn scale(v: f64, host: u32, client: u32) -> i32 {
    let max = f64::from(host - 1);
    (v * f64::from(host) / f64::from(client))
        .round()
        .clamp(0.0, max) as i32
}

struct Host {
    bind: SocketAddr,
    expect: u64,
    idle: Duration,
    max: Duration,
    width: u32,
    height: u32,
    label: String,
    out: Option<String>,
}

fn run_host(flags: &Flags) -> Result<(), String> {
    let opts = Host {
        bind: flags.parsed("bind", SocketAddr::from(([0, 0, 0, 0], 24803)))?,
        expect: flags.parsed("expect", 10_000u64)?,
        idle: Duration::from_millis(flags.parsed("idle-ms", 2_000u64)?),
        max: Duration::from_secs(flags.parsed("max-s", 120u64)?),
        width: flags.parsed("width", 1920u32)?,
        height: flags.parsed("height", 1080u32)?,
        label: flags.parsed("label", "unlabelled".to_owned())?,
        out: flags.value("out").map(str::to_owned),
    };
    if opts.width == 0 || opts.height == 0 {
        return Err("--width and --height must be non-zero".to_owned());
    }

    let mut rx = InputReceiver::bind(opts.bind, session_cipher())
        .map_err(|e| format!("bind {}: {e}", opts.bind))?;
    rx.set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    let mut injector = XTestInjector::connect(opts.width, opts.height)?;
    let display = std::env::var("DISPLAY").unwrap_or_default();
    println!(
        "host listening on {} (DISPLAY={display}, label={})",
        rx.local_addr().map_err(|e| e.to_string())?,
        opts.label
    );
    io::stdout().flush().map_err(|e| e.to_string())?;

    let started = Instant::now();
    let mut last_packet: Option<Instant> = None;
    let mut latencies_us: Vec<u64> = Vec::with_capacity(usize::try_from(opts.expect).unwrap_or(0));
    let mut negative_samples = 0u64;
    let mut last_injected: Option<u64> = None;

    loop {
        match rx.recv_and_inject(&mut injector) {
            Ok(outcome) => {
                last_packet = Some(Instant::now());
                if let Outcome::Injected {
                    sequence,
                    client_ts_us,
                    injected_us,
                } = outcome
                {
                    match injected_us.checked_sub(client_ts_us) {
                        Some(d) => latencies_us.push(d),
                        None => negative_samples += 1,
                    }
                    last_injected = Some(sequence);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("receive: {e}")),
        }
        if rx.stats().datagrams >= opts.expect {
            break;
        }
        if last_packet.is_some_and(|t| t.elapsed() >= opts.idle) {
            break;
        }
        if started.elapsed() >= opts.max {
            break;
        }
    }

    let stats = rx.stats();
    let accepted = stats.accepted();
    let missing = opts.expect.saturating_sub(accepted);
    let pointer = match last_injected {
        Some(seq) => Some(check_pointer(seq, opts.width, opts.height)?),
        None => None,
    };
    let summary = summarize(&mut latencies_us);

    let latency_json = summary.as_ref().map_or("null".to_owned(), Summary::to_json);
    let p99_below_1ms = summary.as_ref().is_some_and(|s| s.p99 < 1_000);
    let pointer_json = pointer.map_or("null".to_owned(), |p| p.to_json());
    let json = format!(
        "{{\"role\":\"host\",\"label\":\"{}\",\"scope\":\"software-only, one kernel clock, provisional\",\"aead\":\"{:?}\",\"width\":{},\"height\":{},\"expected\":{},\"datagrams\":{},\"injected\":{},\"inject_failures\":{},\"authentication_failures\":{},\"malformed\":{},\"replays\":{},\"missing\":{},\"negative_samples\":{},\"latency_us\":{},\"p99_below_1ms\":{},\"pointer\":{}}}",
        opts.label,
        AeadKind::preferred(),
        opts.width,
        opts.height,
        opts.expect,
        stats.datagrams,
        stats.injected,
        stats.inject_failures,
        stats.authentication_failures,
        stats.malformed,
        stats.replays,
        missing,
        negative_samples,
        latency_json,
        p99_below_1ms,
        pointer_json,
    );

    println!("host summary: {json}");
    if let Some(path) = &opts.out {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, format!("{json}\n")).map_err(|e| format!("write {path}: {e}"))?;
        println!("wrote {path}");
    }
    Ok(())
}

/// Percentiles over the injected events, in microseconds.
struct Summary {
    count: usize,
    min: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    max: u64,
    mean: f64,
}

impl Summary {
    fn to_json(&self) -> String {
        format!(
            "{{\"count\":{},\"min\":{},\"p50\":{},\"p90\":{},\"p99\":{},\"max\":{},\"mean\":{:.1}}}",
            self.count, self.min, self.p50, self.p90, self.p99, self.max, self.mean
        )
    }
}

/// Nearest-rank percentile over a sorted, non-empty slice.
fn nearest_rank(sorted: &[u64], pct: f64) -> u64 {
    let n = sorted.len();
    let rank = ((pct / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

fn summarize(samples: &mut [u64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let sum: u64 = samples.iter().sum();
    Some(Summary {
        count: samples.len(),
        min: samples[0],
        p50: nearest_rank(samples, 50.0),
        p90: nearest_rank(samples, 90.0),
        p99: nearest_rank(samples, 99.0),
        max: samples[samples.len() - 1],
        mean: sum as f64 / samples.len() as f64,
    })
}

/// Where the X pointer ended up, compared with where the last injected event should put it.
struct PointerCheck {
    expected: (i32, i32),
    observed: (i32, i32),
}

impl PointerCheck {
    fn to_json(&self) -> String {
        format!(
            "{{\"expected\":[{},{}],\"observed\":[{},{}],\"match\":{}}}",
            self.expected.0,
            self.expected.1,
            self.observed.0,
            self.observed.1,
            self.expected == self.observed
        )
    }
}

/// Polls the root pointer on a second X connection until it matches, or one second passes.
///
/// XTEST requests from the injector and this query arrive on different connections, so the server
/// may read them in either order. A short retry covers that.
fn check_pointer(sequence: u64, width: u32, height: u32) -> Result<PointerCheck, String> {
    let (px, py) = position(sequence, width, height);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| format!("pointer check: connect to X: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let (root, host_w, host_h) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        let reply = conn
            .query_pointer(root)
            .map_err(|e| format!("query_pointer: {e}"))?
            .reply()
            .map_err(|e| format!("query_pointer reply: {e}"))?;
        let check = PointerCheck {
            expected: (
                scale(px, u32::from(host_w), width),
                scale(py, u32::from(host_h), height),
            ),
            observed: (i32::from(reply.root_x), i32::from(reply.root_y)),
        };
        if check.expected == check.observed || Instant::now() >= deadline {
            return Ok(check);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct Client {
    to: SocketAddr,
    bind: SocketAddr,
    count: u64,
    rate_hz: f64,
    width: u32,
    height: u32,
}

fn run_client(flags: &Flags) -> Result<(), String> {
    let opts = Client {
        to: flags
            .value("to")
            .ok_or("--to host:port is required")?
            .parse()
            .map_err(|e| format!("--to: {e}"))?,
        bind: flags.parsed("bind", SocketAddr::from(([0, 0, 0, 0], 0)))?,
        count: flags.parsed("count", 10_000u64)?,
        rate_hz: flags.parsed("rate-hz", 1_000.0f64)?,
        width: flags.parsed("width", 1920u32)?,
        height: flags.parsed("height", 1080u32)?,
    };
    if !(opts.rate_hz.is_finite() && opts.rate_hz > 0.0) {
        return Err("--rate-hz must be a positive number".to_owned());
    }
    if opts.width == 0 || opts.height == 0 {
        return Err("--width and --height must be non-zero".to_owned());
    }

    let mut tx = InputSender::bind(opts.bind, opts.to, session_cipher())
        .map_err(|e| format!("bind {}: {e}", opts.bind))?;
    let local = tx.local_addr().map_err(|e| e.to_string())?;

    let start = Instant::now();
    let mut max_late = Duration::ZERO;
    for i in 0..opts.count {
        // Pace on absolute deadlines, so one late wake does not shift every later event.
        let target = start + Duration::from_secs_f64(i as f64 / opts.rate_hz);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
        max_late = max_late.max(Instant::now().saturating_duration_since(target));

        let (x, y) = position(i, opts.width, opts.height);
        let event = InputEvent {
            kind: InputEventKind::Motion {
                dx: 0.0,
                dy: 0.0,
                x,
                y,
            },
            received_us: now_us(),
        };
        tx.send(&event).map_err(|e| format!("event {i}: {e}"))?;
    }
    println!(
        "client from {local} sent {} events to {} in {:.3} s (sealed {}), max schedule lateness {} us",
        opts.count,
        opts.to,
        start.elapsed().as_secs_f64(),
        tx.sealed(),
        max_late.as_micros()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_picks_the_expected_sample() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&v, 50.0), 50);
        assert_eq!(nearest_rank(&v, 99.0), 99);
        assert_eq!(nearest_rank(&v, 100.0), 100);
    }

    #[test]
    fn summary_reports_extremes() {
        let mut v = vec![5u64, 1, 9, 3];
        let s = summarize(&mut v).expect("non-empty");
        assert_eq!((s.min, s.max, s.count), (1, 9, 4));
    }

    #[test]
    fn positions_stay_on_the_surface() {
        for i in 0..10_000 {
            let (x, y) = position(i, 1920, 1080);
            assert!((0.0..1920.0).contains(&x));
            assert!((0.0..1080.0).contains(&y));
        }
    }

    #[test]
    fn scaling_is_the_identity_at_matching_size() {
        assert_eq!(scale(1234.0, 1920, 1920), 1234);
        assert_eq!(scale(5000.0, 1920, 1920), 1919);
    }
}
