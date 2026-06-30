# Changelog

@htmlonly
<script type="module" src="https://md-block.verou.me/md-block.js"></script>
<md-block
  hmin="2"
  src="https://raw.githubusercontent.com/LizardByte/Lumen/changelog/CHANGELOG.md">
</md-block>
@endhtmlonly

<div class="section_buttons">

| Previous                              |                          Next |
|:--------------------------------------|------------------------------:|
| [Getting Started](getting_started.md) | [Docker](../DOCKER_README.md) |

## Lumen (this fork)

### 2026.999.0 — initial Lumen release

Forked from SolarFlare (`vindeckyy/Solar-Flare`, upstream of
LizardByte/Sunshine). The streaming engine is unchanged; the
surrounding product layer has been rewritten.

**Added**
- Lumen rebrand: `Lumen` everywhere (binary, systemd unit, package,
  web UI, docs, namespaces). One name, end to end.
- REST API surface, documented as OpenAPI 3.1 at
  [`lumen_api/openapi.yaml`](../lumen_api/openapi.yaml). New v1
  endpoints: `GET /api/v1/system/version`, `GET /api/v1/metrics`
  (Prometheus, unauthenticated), `GET /api/v1/sessions`,
  `GET /api/v1/sessions/{id}`.
- `lumenctl` — stdlib-Python CLI client (`lumenctl status`, `apps`,
  `launch`, `config-get`, `config-set`, `clients`, `metrics`,
  `sessions`, `library`, `init`). Zero runtime dependencies, just
  `chmod +x`.
- `tools/lumen_library.py` — game library auto-discovery: scans
  Steam (`libraryfolders.vdf` + `appmanifest_*.acf`), Heroic
  Games Launcher (`sideload_apps/` JSON), Flatpak (`flatpak list
  --app`), and system `.desktop` files. Hand-rolled VDF parser
  (stdlib only). Idempotent and unit-tested with synthetic fixtures.
- `lumenctl init` — headless first-run setup. Writes `lumen.conf`
  + credentials file (`mode 0600`) without a browser. Refuses to
  overwrite without `--force`. Solves container / SSH / cloud-VM
  deployment pain.
- `lumenctl library` — discovers installed games and prints them
  as JSON or a table. `--diff <lumen.conf>` filters out games
  already imported.
- `tools/migrate_sunshine_config.py` — one-shot `sunshine.conf` →
  `lumen.conf` migration. Idempotent, seeds the five Lumen tunables
  (`busy_poll_us`, `enet_4mib_buffer`, `pipewire_latency_ms`,
  `cpu_pinning`, `rate_cap_pct`) and the new `nv_preset` knob.
- `tools/lint_openapi.py` — regex-based OpenAPI sanity checker.
  Stdlib only.
- Prometheus metrics in `src/lumen_metrics.{h,cpp}`: `lumen_up`,
  `lumen_active_streams`, `lumen_bytes_sent`, `lumen_frames_encoded`,
  `lumen_frames_dropped`, `lumen_http_requests`.
- Per-session streaming metrics in `src/lumen_session.{h,cpp}`:
  thread-safe registry exposing `start()` / `record_sample()` /
  `stop()` / `list_active_json()` / `get_json()`. The encode loop
  pokes `record_sample()` once per frame; the new endpoints serve
  the snapshots.
- **Live metrics dashboard** at `/metrics.html` — vanilla-JS page
  (no framework, no chart library) that polls `/api/v1/sessions`
  every second and renders three canvas time-series charts
  (FPS / bitrate / RTT over 60s) plus a per-session table.
  ~180 lines of JS, zero npm dependencies.
- `tests/lumen_api/test_lumen.py` — 11 tests covering the linter,
  the migrator, and the lumenctl shape. All green.
- `tests/lumen_api/test_library_and_init.py` — 14 tests covering
  the VDF parser, Steam scanner, desktop-file scanner, and
  `lumenctl init` (write/overwrite/force).
- `.github/workflows/ci-lumen-python.yml` — runs the Python tooling
  tests on every PR touching `tools/`, `lumen_api/`, `lumenctl/`,
  `tests/lumen_api/`, `docs/api.md`. ~10 s runtime.
- Docs: rewritten `README.md`, new `docs/api.md` (with live-sessions
  section), new `docs/getting_started.md` (with `init` + `library`
  + dashboard quickstart).

**Changed**
- CMake project: `project(Lumen ...)` (was `Sunshine`).
- `package.json`: name `lumen`, added `lint:openapi` script.

**Removed**
- Vendored SolarFlare historical artifacts that only made sense in
  the old fork lineage: `PUSH-INSTRUCTIONS.md`, `cachyos-fastpath.patch`,
  `AGENTS.md`, `.gitmodules`, all submodules under `third-party/` and
  `packaging/linux/flatpak/deps/`. The cachyos tuning knobs are now
  first-class in-tree config keys.

</div>

<details style="display: none;">
  <summary></summary>
  [TOC]
</details>
