# SolarFlare fork modules

> Six small C++ modules sit on top of the upstream Sunshine fork.
> ponytail: each module is the minimum code that ships a usable
> subsystem; if you don't need one, the corresponding `enabled`
> config key turns it off at runtime without rebuilding.

| Module | One-liner |
|---|---|
| `solarflare::telemetry` | per-stage latency min/max/avg + throughput atomics + a session-info slot. One observer slot for downstream consumers. |
| `solarflare::adaptive_bitrate` | subscribes to telemetry, drops the negotiated bitrate when RTT or loss crosses a threshold. Honours cooldown so it can't oscillate. |
| `solarflare::health_monitor` | one worker thread sampling CPU / memory / thermal / disk; raises warnings + criticals with per-(resource,severity) cooldown. |
| `solarflare::network_probe` | per-target RTT p95 + jitter + loss + a single composite score function. |
| `solarflare::session_recorder` | writes one JSON per session to `<appdata>/solarflare/sessions/`, plus an `index.json` capped at 500 records. |
| `solarflare::app_profiler` | JSON profile list, matches exec basename against each profile's `match` substring, applies per-game overrides. |
| `solarflare::codec_presets` | 19 named `(name, encoder, codec, kv)` quality presets. |
| `solarflare::cli` (`solarctl`) | `sunshine solarctl <verb>` thin wrapper over the C++ modules. |

The `solarflare::runtime` namespace owns the lifecycle (`init()` /
`shutdown()`). `main.cpp` calls it once around the HTTP interface
initialisation.

## Wiring

```cpp
// main.cpp
#include "solarflare/runtime.h"
...
solarflare::runtime::init();          // after logging, before HTTP
...                                    // main loop
solarflare::runtime::shutdown();      // before process exit
```

The HTTP API handlers (`/api/solarflare/*`) are wired into
`confighttp::start()` and registered alongside the upstream routes
— same auth, same CSRF, same origin policy.

## Config

All knobs live under `[solarflare]` in `~/.config/sunshine/sunshine.conf`.
See `docs/CONFIGURATION.md` for the full table. Master enables:

- `telemetry_enabled` (default `true`)
- `adaptive_bitrate_enabled` (default `true`)
- `health_monitor_enabled` (default `true`)
- `session_recorder_enabled` (default `true`)
- `app_profiler_enabled` (default `true`)

Set any to `false` to drop that subsystem with no rebuild.

## CLI

```
$ sunshine solarctl status
$ sunshine solarctl telemetry
$ sunshine solarctl health
$ sunshine solarctl sessions 20
$ sunshine solarctl profiles
$ sunshine solarctl presets "NVENC Low Latency 1080p60"
$ sunshine solarctl network
$ sunshine solarctl help
```

## Where the data flows

```
   capture → color_convert → encode → pacer → send → wire
      ↓           ↓            ↓        ↓       ↓
  telemetry::record_latency(STAGE_CAPTURE)
                          telemetry::record_latency(STAGE_COLOR_CONVERT)
                          telemetry::record_latency(STAGE_ENCODE)         ← stream.cpp
                          telemetry::record_latency(STAGE_PACER)
                          telemetry::record_latency(STAGE_SEND)            ← stream.cpp
                          telemetry::record_latency(STAGE_NETWORK_RTT)
                          telemetry::record_send(bytes, lost_delta)
                          telemetry::record_frame()

   ↓
   snapshot_t   ←— adaptive_bitrate::evaluate()    ←— config::solarflare
                       ↓                                       (drop bitrate step)
                       ↓
                observer callback fires synchronously
                       ↓
   GET /api/solarflare/telemetry  ←— web UI polls every 500 ms
```

## Tests

`tests/unit/test_solarflare_modules.cpp` -- one assertion per module
(round-trip, boundary, lifecycle). Per ponytail: non-trivial logic
gets one runnable check, no per-edge-case matrix.

## See also

- `docs/solarflare/WEB_UI.md` — the eight pages the web UI ships
- `docs/solarflare/CODECS.md` — the 19 named codec presets
- `docs/CONFIGURATION.md` — full config table (upstream + SolarFlare)
- `docs/PORTING.md` — multi-distro build instructions
