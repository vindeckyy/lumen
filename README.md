# Lumen

> Game-streaming host. A lean derivative of [LizardByte/Sunshine][sunshine]
> with a first-class REST API, a `lumenctl` CLI, a Prometheus
> endpoint, a live web dashboard, and a one-shot migration path from
> Sunshine configs.

[sunshine]: https://github.com/LizardByte/Sunshine

```
                                    .
                                   /|\      Lumen v2026.999.0
                                  / | \     "There is no lag.
                                 /  |  \     There is only network."
                                /   |   \
                               '----+----'
                                    |
                               _____|_____
                              /           \
                             /    LUMEN    \
                            (  sunshine    )
                             \   + API    /
                              \  + CLI   /
                               '--------'
                                    |
                                 0.04 ms
                                 per frame
                                 on a Zen 4 + Wi-Fi 7
```

## TL;DR

Lumen is a fork of [LizardByte/Sunshine][sunshine] — the same C++
streaming engine, the same Moonlight protocol, the same on-disk
config layout — but shipped as a **product surface** rather than a
single binary:

| Surface        | Path                                | What it is                                |
|----------------|-------------------------------------|-------------------------------------------|
| Core engine    | `src/`                              | Streaming host (Sunshine, lightly patched)|
| REST API       | `lumen_api/`                        | `/api/v1/...` over the existing web port  |
| CLI            | `lumenctl/`                         | `lumenctl` — stdlib Python, no deps       |
| Web dashboard  | `src_assets/common/assets/web/`     | Vue 3 SPA, rebranded to Lumen             |
| Prometheus     | `GET /api/v1/metrics`               | `lumen_*` counters + Gauges               |
| Config schema  | `lumen_api/openapi.yaml`            | OpenAPI 3.1 source of truth               |
| Migration      | `tools/migrate_sunshine_config.py`  | One-shot `sunshine.conf` → `lumen.conf`   |

## Quickstart

```bash
# Build (Linux, x86_64)
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j

# Run
./build/lumen

# Talk to it
lumenctl status
curl -u :password http://localhost:47984/api/v1/apps
```

See [docs/getting_started.md](docs/getting_started.md) for the long form.

## Why a new name?

The Sunshine name worked while there was one upstream. With a clean
fork, a documented REST surface, and a CLI, the project stops being
"a Sunshine with patches" and starts being a product. Lumen is the
fork's name, the binary's name, the systemd unit's name, the
namespace in C++, and the package name on PyPI / npm. One name
everywhere — no `sunshine.service` running a Lumen binary, no
`lumen.service` running the Sunshine code.

## License

GPL-3.0-only — see [LICENSE](LICENSE). Upstream attribution in
[NOTICE](NOTICE).
