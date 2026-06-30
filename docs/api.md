# Lumen REST API

Lumen exposes an HTTP API on the same port as the web UI
(`https://localhost:47984` by default, self-signed cert).
Every endpoint under `/api/*` requires HTTP Basic auth using the
same credentials as the web UI. The one exception is
`/api/v1/metrics`, which is intentionally unauthenticated so
scrapers can fetch it without managing creds.

The full machine-readable contract is [`lumen_api/openapi.yaml`](../lumen_api/openapi.yaml).
The CLI in [`lumenctl/`](../lumenctl/) is a stdlib-Python reference
client and the fastest way to poke the API.

## Endpoints (summary)

| Method | Path | Purpose |
|--------|------|---------|
| `GET`   | `/api/apps`                  | List configured apps               |
| `POST`  | `/api/apps`                  | Create or overwrite an app         |
| `DELETE`| `/api/apps/{index}`          | Delete an app by index             |
| `POST`  | `/api/apps/close`            | Terminate the running app          |
| `GET`   | `/api/config`                | Read full config tree              |
| `POST`  | `/api/config`                | Replace the config tree            |
| `GET`   | `/api/clients/list`          | List paired Moonlight clients      |
| `POST`  | `/api/clients/unpair`        | Unpair a single client             |
| `POST`  | `/api/clients/unpair-all`    | Unpair every client                |
| `POST`  | `/api/clients/update`        | Enable / disable a paired client   |
| `POST`  | `/api/password`              | Change the web-UI password         |
| `POST`  | `/api/pin`                   | Pair with a PIN                    |
| `POST`  | `/api/restart`               | Restart the streaming host         |
| `GET`   | `/api/logs`                  | Tail the in-memory log buffer      |
| `GET`   | `/api/v1/system/version`     | **New in Lumen** — build info      |
| `GET`   | `/api/v1/metrics`            | **New in Lumen** — Prometheus      |
| `GET`   | `/api/v1/sessions`           | **New in Lumen** — active streams  |
| `GET`   | `/api/v1/sessions/{id}`      | **New in Lumen** — one stream      |

## `lumenctl` examples

```bash
# Show Lumen version + build
lumenctl status

# List configured apps
lumenctl apps

# Patch a single config key (RFC 7396 merge-patch)
echo '{"lumen":{"nv_preset":0}}' > /tmp/patch.json
lumenctl config-set /tmp/patch.json

# Scrape Prometheus metrics, filter to active streams
lumenctl metrics --filter lumen_active_streams
```

## Auth

`lumenctl` reads the password via `getpass` on every invocation
(no env var, no file). If you want daemon-mode operation, wrap
it in a keyring helper; we deliberately don't ship one to keep
the dependency surface to zero.

## Metrics

`GET /api/v1/metrics` returns Prometheus text format
(`text/plain; version=0.0.4`). Counter and gauge names are
prefixed `lumen_`. Example:

```text
# HELP lumen_up Whether the streaming host is alive (1) or not (0).
# TYPE lumen_up gauge
lumen_up 1
# HELP lumen_active_streams Number of streams currently active.
# TYPE lumen_active_streams gauge
lumen_active_streams 0
# HELP lumen_bytes_sent Total bytes sent to clients since process start.
# TYPE lumen_bytes_sent counter
lumen_bytes_sent 0
```

Configure your Prometheus scrape like so:

```yaml
scrape_configs:
  - job_name: lumen
    scheme: https
    tls_config:
      insecure_skip_verify: true   # self-signed cert
    static_configs:
      - targets: ['localhost:47984']
    metrics_path: /api/v1/metrics
```

## Versioning

Path versioning (`/api/v1/...`) is the Lumen convention. New
fields may be added to existing schemas without bumping the
major; breaking changes move to `/api/v2/...`. The legacy
`/api/...` paths follow the Sunshine semantic and are stable.

## Live sessions

`GET /api/v1/sessions` returns the per-stream metrics the
`/metrics.html` dashboard renders. A session entry looks like:

```json
{
  "id": "5e1c…",
  "app_name": "Elden Ring",
  "client_name": "Living Room PC",
  "started_at_ms": 1719763912000,
  "encoder_fps": 143.7,
  "target_fps": 144,
  "bitrate_kbps": 48230,
  "rtt_ms": 18,
  "encode_ms": 4.7,
  "frames_sent": 183921,
  "frames_dropped": 42
}
```

The web dashboard polls this endpoint every second. For
command-line users:

```bash
lumenctl sessions
#   5e1c…          Elden Ring               Living Room PC    fps=143.7  bitrate=48230kbps  rtt=  18ms
```

The dashboard at `/metrics.html` is a self-contained vanilla-JS
page (no chart library, no framework) — open it in any browser
to see FPS, bitrate, and RTT over the last 60 seconds.