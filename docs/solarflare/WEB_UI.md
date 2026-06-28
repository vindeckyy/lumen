# Web UI

Lumen ships eight pages on top of upstream Sunshine's web UI.

| Page | Path | What it shows |
|---|---|---|
| Live Dashboard | `/dashboard` | Real-time FPS EWMA, kbps sparklines, per-stage latency bars, current session card |
| Clients | `/sf-clients` | Paired Moonlight clients + active session count + one-click disconnect-all |
| App Profiles | `/profiles` | Per-game config overrides (FPS / resolution / bitrate / encoder / codec) with CRUD modal editor |
| Sessions | `/sessions` | Historical streaming sessions with delete-per-row |
| Health | `/sf-health` | CPU / memory / disk / thermal gauges + recent alerts |
| Network | `/sf-network` | Per-target RTT p95 / jitter / loss / score bars |
| Codec Presets | `/codec-presets` | 19 named quality presets across NVENC/AMF/VAAPI/software/VT/QSV/Vulkan, filterable by encoder |
| Setup Wizard | `/wizard` | 5-step first-time setup checklist (password, tuning, profiles, presets, pairing) |

All eight are reachable from the **SolarFlare** dropdown in the navbar.

## Polling

The dashboard and live pages poll `/api/solarflare/*` every 500 ms
(dashboard) or 2 s (everything else). No SSE, no WebSocket — the
Simple-Web-Server in upstream Sunshine doesn't ship them and the
polling cost is small (single-digit KB per tick).

## Authentication

Every SolarFlare HTTP endpoint uses the same auth scheme as the
upstream `/api/config` endpoint. If you can hit `config.html`, you
can hit `dashboard.html`.

## Styling

All eight pages use three new utility classes added to
`sunshine.css`:

- `.solarflare-card` — soft border + hover shadow
- `.solarflare-status-dot` — coloured dot with `ok / warn / error / unknown`
- `.modal-backdrop-solarflare` — backdrop for the profiles editor modal

The navbar wordmark gradient and SolarFlare Dark / SolarFlare Light
themes from the previous fork are preserved.

## File map

```
src_assets/common/assets/web/
├── dashboard.html            Vue 3 SPA, polls /api/solarflare/telemetry every 500 ms
├── sf-clients.html           Vue 3 SPA, polls /api/solarflare/clients every 2 s
├── profiles.html             Vue 3 SPA + modal editor
├── sessions.html             Vue 3 SPA
├── sf-health.html            Vue 3 SPA
├── sf-network.html           Vue 3 SPA
├── codec-presets.html        Vue 3 SPA
├── wizard.html               Static page (no Vue state)
├── Navbar.vue                Updated: SolarFlare dropdown
├── sunshine.css              +53 lines: solarflare-card / status-dot / modal-backdrop
└── public/assets/locale/en.json   +8 top-level keys (dashboard / sf_clients / ...)
```

Each page is a single-file Vue 3 SPA with inline `<script
type="module">`. They share the `initApp` helper from `init.js`.
