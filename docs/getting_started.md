# Getting Started

Lumen is a fork of [LizardByte/Sunshine][sunshine] — the same C++
game-streaming host, repackaged as a product with a documented
HTTP API, a CLI, and a Prometheus endpoint.

[sunshine]: https://github.com/LizardByte/Sunshine

## Install (Linux)

Pre-built packages aren't shipped yet (Lumen is brand-new as a
fork). Build from source:

```bash
git clone https://github.com/vindeckyy/lumen.git
cd lumen
cmake -B build -DCMAKE_BUILD_TYPE=Release \
                -DSUNSHINE_CACHYOS_NATIVE=ON   # optional, x86_64 Zen tuning
cmake --build build -j
sudo ./scripts/linux_build.sh install   # installs binary, systemd unit, web UI
```

## Run

```bash
./build/lumen
# -> Configuration UI available at [https://localhost:47984]
```

Pair a Moonlight client by visiting `https://<host>:47984` in a
browser, setting a username/password, and entering the PIN shown
in Moonlight.

## Configure

Lumen reads `~/.config/lumen/lumen.conf` (created on first
launch). See [`configuration.md`](configuration.md) for the
full reference. Migrating from Sunshine?

```bash
tools/migrate_sunshine_config.py ~/.config/sunshine/sunshine.conf \
        > ~/.config/lumen/lumen.conf
```

The migration is idempotent — running it twice on the same input
produces the same output.

## Use the CLI

```bash
# Install the CLI (no deps, stdlib only)
install -m755 lumenctl/lumenctl ~/.local/bin/lumenctl

# Show the Lumen build info
lumenctl status

# List paired clients
lumenctl clients

# Discover installed games (Steam / Heroic / Flatpak / system .desktop)
lumenctl library --pretty

# Headless first-run setup (no browser needed — for containers, SSH, cloud VMs)
lumenctl init --user admin --password 'redsox!2621' --config-dir ~/.config/lumen
```

`lumenctl` prompts for the web-UI password on every call unless
you used `init` (which writes the credentials file directly).
See [`api.md`](api.md) for the full API surface.

## Live metrics dashboard

Open `https://<host>:47984/metrics.html` after pairing a client.
You'll see three live charts (FPS / bitrate / RTT over the last
60 seconds) and a per-session table. The page polls
`/api/v1/sessions` every second — no WebSocket, no framework,
just canvas + `setInterval`.

## Scrape metrics

```bash
curl -k https://localhost:47984/api/v1/metrics
```

Or point Prometheus at `/api/v1/metrics` directly
(see [`api.md`](api.md#metrics) for the scrape config).

## Where to go next

- [`api.md`](api.md) — full API reference + `lumenctl` examples
- [`configuration.md`](configuration.md) — every config knob
- [`performance_tuning.md`](performance_tuning.md) — CachyOS tuning
- [`PORTING.md`](PORTING.md) — what changed vs upstream Sunshine