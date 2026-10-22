//! The host's control plane.
//!
//! The dashboard asks two things of the host: what is the state of the
//! session, and apply this change. The state is read from the pipeline's
//! own counters, the stage timings, the session table, the output list and
//! the pairing store. The changes go through the same settings the stages
//! read, so a change the dashboard makes is a change the pipeline sees.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};

use lumen_web::{Api, ApiError};

use crate::monitors::MonitorInfo;
use crate::multi::SessionTable;
use crate::pairing::PairingStore;
use crate::pipeline::{Counters, HostConfig, snapshot};
use crate::settings::{HostSettings, ParityMode};
use crate::stages::StageRecorder;

/// The key length of a device key.
const KEY_LEN: usize = 32;

/// How many latency samples the history keeps.
const HISTORY: usize = 240;

/// The recent latency samples, for the chart.
///
/// The history lives in the host, not in the browser: a chart that starts
/// empty every time the page is opened cannot show what the session has
/// been doing.
#[derive(Debug, Default)]
pub struct History {
    /// The samples, oldest first: the p50 and the p99 in milliseconds.
    samples: Mutex<VecDeque<(f64, f64)>>,
}

impl History {
    /// Add a sample.
    pub fn push(&self, p50: f64, p99: f64) {
        if let Ok(mut samples) = self.samples.lock() {
            samples.push_back((p50, p99));
            while samples.len() > HISTORY {
                samples.pop_front();
            }
        }
    }

    /// The samples, oldest first.
    #[must_use]
    pub fn samples(&self) -> Vec<(f64, f64)> {
        self.samples
            .lock()
            .map(|samples| samples.iter().copied().collect())
            .unwrap_or_default()
    }
}

/// The actions the control plane asks the pipeline to carry out.
#[derive(Debug, Default)]
pub struct Actions {
    /// The sessions to end, by identifier.
    pub kick: Mutex<Vec<u32>>,
    /// The clients to stop trusting, by key.
    pub revoke: Mutex<Vec<[u8; KEY_LEN]>>,
    /// The picture the operator asked for.
    pub quality: Mutex<Option<crate::settings::Quality>>,
}

impl Actions {
    /// Take the sessions to end.
    pub fn take_kicks(&self) -> Vec<u32> {
        self.kick
            .lock()
            .map(|mut k| std::mem::take(&mut *k))
            .unwrap_or_default()
    }

    /// Take the clients to stop trusting.
    pub fn take_revocations(&self) -> Vec<[u8; KEY_LEN]> {
        self.revoke
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }

    /// Ask for a session to end.
    pub fn kick(&self, id: u32) {
        if let Ok(mut kick) = self.kick.lock() {
            kick.push(id);
        }
    }

    /// Ask for a picture size.
    pub fn set_quality(&self, quality: crate::settings::Quality) {
        if let Ok(mut slot) = self.quality.lock() {
            *slot = Some(quality);
        }
    }

    /// Take the picture the operator asked for.
    pub fn take_quality(&self) -> Option<crate::settings::Quality> {
        self.quality.lock().ok().and_then(|mut q| q.take())
    }

    /// Ask for a client to stop being trusted.
    pub fn revoke(&self, key: [u8; KEY_LEN]) {
        if let Ok(mut revoke) = self.revoke.lock() {
            revoke.push(key);
        }
    }
}

/// Everything the control plane can see and change.
#[derive(Clone, Debug)]
pub struct HostView {
    /// The counters of the pipeline.
    pub counters: Arc<Counters>,
    /// The per-stage timings.
    pub stages: Arc<Mutex<StageRecorder>>,
    /// The settings the stages read.
    pub settings: Arc<HostSettings>,
    /// The sessions the host serves.
    pub sessions: Arc<Mutex<SessionTable>>,
    /// The outputs the host can capture.
    pub monitors: Arc<Mutex<Vec<MonitorInfo>>>,
    /// The pairing store.
    pub pairing: Arc<Mutex<PairingStore>>,
    /// The actions the pipeline carries out.
    pub actions: Arc<Actions>,
    /// The host's configuration.
    pub config: HostConfig,
    /// When the host started.
    pub started: Instant,
    /// The pairing code the host is showing, when there is one.
    pub pairing_code: Arc<Mutex<Option<u32>>>,
    /// Whether the capture is running.
    pub capture_running: Arc<AtomicBool>,
    /// The latency history, for the chart.
    pub history: Arc<History>,
}

impl HostView {
    /// The percentiles of a set of samples.
    fn percentiles(samples: &[i64]) -> Value {
        if samples.is_empty() {
            return json!({ "p50": 0, "p90": 0, "p99": 0, "max": 0, "count": 0 });
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let at = |q: f64| -> i64 {
            let index = ((sorted.len() as f64 - 1.0) * q).round() as usize;
            sorted[index.min(sorted.len() - 1)]
        };
        json!({
            "p50": at(0.50) as f64 / 1000.0,
            "p90": at(0.90) as f64 / 1000.0,
            "p99": at(0.99) as f64 / 1000.0,
            "max": *sorted.last().unwrap_or(&0) as f64 / 1000.0,
            "count": sorted.len(),
        })
    }

    /// The per-stage percentiles, in milliseconds.
    fn latency(&self) -> Value {
        let samples = self
            .stages
            .lock()
            .map(|mut stages| stages.samples().to_map())
            .unwrap_or_default();
        let mut out = serde_json::Map::new();
        for (stage, values) in &samples {
            out.insert(stage.clone(), Self::percentiles(values));
        }
        Value::Object(out)
    }

    /// The time the frames were counted over.
    ///
    /// The host's own duration counter is only final when the run ends, so
    /// the rate the dashboard shows would be wrong while the run is still
    /// going. The counters carry the frames, and the time they were
    /// streamed over is what the rate has to be measured against.
    fn streamed_for(&self) -> f64 {
        self.stages.lock().map_or(0.0, |stages| stages.session().1)
    }

    /// The frames the session has delivered, and the rate they came at.
    fn delivered(&self) -> (u64, f64) {
        let (frames, span) = self
            .stages
            .lock()
            .map_or((0, 0.0), |stages| stages.session());
        let rate = if span > 0.0 {
            frames as f64 / span
        } else {
            0.0
        };
        (frames, rate)
    }

    /// The state of the host.
    fn state_json(&self) -> Value {
        let stats = snapshot(&self.counters, BTreeMap::new());
        let settings = &self.settings;
        let sessions = self.sessions.lock().map_or_else(
            |_| Vec::new(),
            |table| {
                table
                    .sessions()
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "role": s.role.name(),
                            "peer": s.peer.to_string(),
                            "media_port": s.media_port,
                            "audio_port": s.audio_port,
                            "input": s.role.has_input(),
                        })
                    })
                    .collect::<Vec<Value>>()
            },
        );
        let outputs = self.monitors.lock().map_or_else(
            |_| Vec::new(),
            |list| {
                list.iter()
                    .map(|m| {
                        json!({
                            "name": m.name,
                            "width": m.width,
                            "height": m.height,
                            "refresh_hz": m.refresh_hz,
                            "primary": m.primary,
                            "active": self.config.output.as_deref() == Some(m.name.as_str())
                                || (self.config.output.is_none() && m.index == 0),
                        })
                    })
                    .collect::<Vec<Value>>()
            },
        );
        let (code, trusted) = self.pairing.lock().map_or((0, Vec::new()), |store| {
            let trusted = store
                .trusted_clients()
                .iter()
                .map(|key| {
                    let hex = crate::pairing::hex_encode(key);
                    json!({ "key": hex, "short": hex[..12.min(hex.len())].to_string() })
                })
                .collect::<Vec<Value>>();
            (store.trusted_count(), trusted)
        });
        let host_key = self
            .pairing
            .lock()
            .map_or_default(|store| crate::pairing::hex_encode(&store.public_bytes()));
        let pairing_code = self
            .pairing_code
            .lock()
            .ok()
            .and_then(|code| *code)
            .map(|code| format!("{code:06}"))
            .unwrap_or_default();

        json!({
            "host": {
                "name": "lumen-host",
                "uptime_s": self.started.elapsed().as_secs_f64(),
                // The time the frames were counted over, which is not the
                // time the host has been up: the client may have come
                // later.
                "streamed_s": self.streamed_for(),
                "capture_backend": format!("{:?}", self.config.capture).to_lowercase(),
                "display": self.config.display.clone().unwrap_or_else(|| "the environment".to_string()),
                "control_addr": self.config.control_addr.to_string(),
                "capture_running": self.capture_running.load(Ordering::Relaxed),
            },
            "stream": {
                // The picture the host serves now, which is not the one it
                // captures: a client that cannot keep up asked for a smaller
                // one.
                "width": settings.quality().width,
                "height": settings.quality().height,
                "capture_width": self.config.width,
                "capture_height": self.config.height,
                "fps": settings.fps(),
                "qp": settings.qp(),
                "bitrate_bps": settings.bitrate_bps(),
                "bitrate_mbps": f64::from(settings.bitrate_bps()) / 1_000_000.0,
                "rate_control": if settings.constant_quantiser() { "cqp" } else { "cbr" },
                "parity": settings.parity().name(),
                "audio": settings.audio(),
                "adaptive_bitrate": settings.adaptive_bitrate(),
                "capture_backend": format!("{:?}", self.config.capture).to_lowercase(),
            },
            "counters": {
                "frames_captured": stats.frames_captured,
                "frames_encoded": stats.frames_encoded,
                "datagrams_sent": stats.datagrams_sent,
                "bytes_sent": stats.bytes_sent,
                "capture_errors": stats.capture_errors,
                "encode_errors": stats.encode_errors,
                "keyframe_requests": stats.keyframe_requests,
                "retransmits": stats.retransmits,
                "audio_packets": stats.audio_packets,
                "clipboard_roundtrips": stats.clipboard_roundtrips,
                "resize_events": stats.resize_events,
                "parity_level_changes": stats.parity_level_changes,
                "estimated_bytes_per_sec": stats.estimated_bytes_per_sec,
                "duration_s": stats.duration_ms as f64 / 1000.0,
                "streamed_s": self.streamed_for(),
                // The rate the pipeline delivered, not the rate that was
                // asked for.
                "delivered_fps": self.delivered().1,
            },
            "latency": self.latency(),
            "history": self
                .history
                .samples()
                .into_iter()
                .map(|(p50, p99)| json!([p50, p99]))
                .collect::<Vec<Value>>(),
            "sessions": sessions,
            "outputs": outputs,
            "pairing": {
                "code": pairing_code,
                "host_key": host_key,
                "trusted": trusted,
                "trusted_count": code,
            },
        })
    }
}

/// The control plane's view of the host.
#[derive(Debug)]
pub struct HostApi {
    /// The host behind it.
    view: HostView,
}

impl HostApi {
    /// Wrap a view of the host.
    #[must_use]
    pub fn new(view: HostView) -> Self {
        Self { view }
    }
}

impl Api for HostApi {
    fn state(&self) -> Value {
        let mut state = self.view.state_json();
        if let Value::Object(map) = &mut state {
            map.insert("log".to_string(), Value::Array(log_lines()));
        }
        state
    }

    fn command(&self, name: &str, args: &Value) -> Result<Value, ApiError> {
        let settings = &self.view.settings;
        match name {
            "set_parity" => {
                let mode = args
                    .get("mode")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants a mode"))?;
                let mode = ParityMode::parse(mode).ok_or_else(|| {
                    ApiError::new("bad_request", format!("there is no parity mode {mode}"))
                })?;
                settings.set_parity(mode);
                Ok(json!({ "ok": true, "message": format!("the parity is now {}", mode.name()) }))
            }
            "apply_encoder" => {
                if let Some(mode) = args.get("mode").and_then(Value::as_str) {
                    match mode {
                        "cqp" => settings.set_constant_quantiser(true),
                        "cbr" => settings.set_constant_quantiser(false),
                        other => {
                            return Err(ApiError::new(
                                "bad_request",
                                format!("there is no rate control called {other}"),
                            ));
                        }
                    }
                }
                if let Some(qp) = args.get("qp").and_then(Value::as_u64) {
                    settings.set_qp(qp as u32);
                }
                if let Some(mbps) = args.get("bitrate_mbps").and_then(Value::as_f64) {
                    settings.set_bitrate_bps((mbps * 1_000_000.0).round() as u32);
                }
                if let Some(fps) = args.get("fps").and_then(Value::as_u64) {
                    settings.set_fps(fps as u32);
                }
                if let Some(adaptive) = args.get("adaptive_bitrate").and_then(Value::as_bool) {
                    settings.set_adaptive_bitrate(adaptive);
                }
                Ok(json!({ "ok": true, "message": "the encoder settings are applied" }))
            }
            "set_audio" => {
                let enabled = args
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants enabled"))?;
                settings.set_audio(enabled);
                Ok(json!({
                    "ok": true,
                    "message": format!("new sessions will {} audio", if enabled { "carry" } else { "not carry" }),
                }))
            }
            "set_quality" => {
                let width = args
                    .get("width")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants a width"))?;
                let height = args
                    .get("height")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants a height"))?;
                let current = self.view.settings.quality();
                // The capture is the ceiling: the host does not invent pixels.
                let quality = crate::settings::Quality::new(
                    u32::try_from(width)
                        .unwrap_or(0)
                        .min(self.view.config.width),
                    u32::try_from(height)
                        .unwrap_or(0)
                        .min(self.view.config.height),
                    current.fps,
                );
                if quality == current {
                    Ok(json!({ "ok": true, "message": "the picture is already that size" }))
                } else {
                    self.view.settings.set_quality(quality);
                    self.view.actions.set_quality(quality);
                    Ok(json!({
                        "ok": true,
                        "message": format!("the picture is now {}x{}", quality.width, quality.height),
                    }))
                }
            }
            "request_idr" => {
                settings.request_keyframe();
                Ok(json!({ "ok": true, "message": "a keyframe was asked for" }))
            }
            "kick_session" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants an id"))?;
                self.view.actions.kick(id as u32);
                Ok(json!({ "ok": true, "message": format!("session {id} is ending") }))
            }
            "revoke_client" => {
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::new("bad_request", "the command wants a key"))?;
                let bytes = crate::pairing::hex_decode(key)
                    .ok_or_else(|| ApiError::new("bad_request", "that is not a key"))?;
                if bytes.len() != KEY_LEN {
                    return Err(ApiError::new(
                        "bad_request",
                        "that key has the wrong length",
                    ));
                }
                let mut raw = [0u8; KEY_LEN];
                raw.copy_from_slice(&bytes);
                self.view.actions.revoke(raw);
                Ok(json!({ "ok": true, "message": "the client is no longer trusted" }))
            }
            _ => Err(ApiError::new(
                "unknown_command",
                format!("there is no command called {name}"),
            )),
        }
    }

    fn log(&self) -> Vec<Value> {
        log_lines()
    }
}

/// The recent log lines, as the control plane sends them.
fn log_lines() -> Vec<Value> {
    crate::log::recent(200)
        .into_iter()
        .map(|line| json!({ "level": line.level, "time": line.time, "message": line.message }))
        .collect()
}
