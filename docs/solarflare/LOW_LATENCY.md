# Low-latency features — what makes this fork worth picking

Five things upstream Sunshine doesn't do, that matter for PC → TV
streaming where every millisecond of input lag is visible.

## 1. Quick Tune — one-click pre-stream prep

The dashboard has a **Quick Tune** button. Click it once and
SolarFlare does four things upstream can't:

| Step | What it does | Why it matters |
|---|---|---|
| `boost_hid_polling` | writes `1000` to `/sys/module/usbhid/parameters/poll` (Linux) | A 1000Hz gaming mouse delivers every sample; the kernel default is 125Hz. |
| `set_gpu_perf` | writes `gt_max_freq_mhz` to `gt_min_freq_mhz` on Intel iGPUs | Stops the iGPU from power-saving mid-stream and costing you 5–10 ms of encode stalls. |
| `inhibit_blanking` | forks a `systemd-inhibit` child that holds the lock for 24 h (`caffeinate -di` on macOS) | Stops the host monitor from blanking when the only display you're looking at is the TV. |
| `mark_qos` | sets DSCP EF (46) on the streaming socket | QoS-aware routers (UniFi, EdgeOS, OpenWrt with sqm) put streaming packets ahead of bulk traffic. |

Each step is independent — a step that fails (e.g. CEC not present)
doesn't roll back the others. Click Quick Tune again to revert.

## 2. TV Game Mode (HDMI ALLM via CEC)

Most TVs from 2019 onwards support **Auto Low Latency Mode** over
HDMI 2.1. ALLM tells the TV to switch to game mode (lower input
lag, less post-processing). With `cec-client` installed, the
dashboard's **TV Game Mode** button sends the CEC `Set Stream Path`
+ ALLM enable message. When the stream ends, click it again to
restore the TV to its prior state.

If `cec-client` isn't on `$PATH`, the button silently disables
itself and the snapshot returns `unsupported` — no errors in the
log.

## 3. Latency Budget

The dashboard renders a stacked bar that splits your input-to-photon
latency into where the time actually goes:

```
[ capture | color | encode | pacer | send | network | decode | display ]
```

The split comes from `telemetry::snapshot_t` for the server side
and a configurable estimate (default: 4 ms decode, 8 ms display) for
the client side. The number at the top is colour-coded green
<30 ms, blue <60 ms, yellow <100 ms, red ≥100 ms.

This is the single most useful tool for tuning — when the number
goes up, you see whether the spike was in encode (CPU), send
(network), or display (TV).

## 4. Inspector + Recommendation

`GET /api/solarflare/inspector` returns one JSON blob describing
what the host detected: CPU model + microarch, GPU vendor +
driver, display server (Wayland/X11/KMS), and a
`recommended.preset` chosen from `codec_presets`.

The dashboard renders this as a header strip above the session
card so you always see "this host is on NVENC HEVC Balanced, set up
for your RTX 3060 + Zen 2 + Wayland" at a glance.

## 5. App Profiles

Per-game overrides that match the launched executable's basename
against a JSON list and apply `fps / width / height / bitrate /
encoder / codec` before the stream starts. See `/profiles` in the
UI or `docs/solarflare/MODULES.md` for the schema.

## Where these live in the code

| Feature | Source |
|---|---|
| Quick Tune | `src/solarflare/performance.{h,cpp}` |
| ALLM / CEC | `src/solarflare/cec.{h,cpp}` |
| Latency budget | `src/solarflare/latency_budget.{h,cpp}` |
| Inspector | `src/solarflare/inspector.{h,cpp}` |
| App profiles | `src/solarflare/app_profiler.{h,cpp}` |
| HTTP API | `src/confighttp.cpp` (12 new endpoints) |
| Dashboard | `src_assets/common/assets/web/dashboard.html` |

All toggled by SolarFlare config keys; see `docs/CONFIGURATION.md`.
