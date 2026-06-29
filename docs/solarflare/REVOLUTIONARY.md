# Revolutionary features — why this fork

Three features **upstream Sunshine doesn't have** that meaningfully
change the PC → TV streaming experience. None of them are
incremental; each one shows you something you've never been able to
see.

## 1. Predictive Adaptive Bitrate (jitter-aware)

**The problem.** Upstream Sunshine (and Moonlight's stock ABR)
drops the bitrate when packet loss shows up. On a wireless link,
that means the user has *already* seen 2–4 dropped frames by the
time the system reacts. The first cut of `adaptive_bitrate` here did
the same thing.

**What SolarFlare does.** It watches the *second derivative* of
RTT: is jitter *accelerating upward*? On Wi-Fi the spike in jitter
arrives 200–400 ms **before** loss shows up, because the queue
fills first and drops second. The predictor drops the bitrate when
`dRTT/dt > 8 ms/s` (configurable) — so the stutter never happens.

Once jitter peaks and RTT settles back, the controller raises by
`step_kbps_up` (1.5 Mbps) with a long `cooldown_up` (8 s) so a
single burst doesn't cause step-wise thrash.

Visible on the dashboard:
```
Predictive ABR: 18432 kbps
  ↓j 3   ↓l 0   ↑ 12
  jitter accel: 1.4 ms/s   (green = good)
```

Code: `src/solarflare/predictive_abr.{h,cpp}`.
API: `GET /api/solarflare/predictive-abr`.

## 2. Stutter Score (frame interval histogram)

**The problem.** Most streaming dashboards show "FPS: 59.5" as if
that's a meaningful number. It's not. What you actually notice on
a TV is the occasional hitch — one frame that takes 80 ms instead
of 16.67, every 3–10 seconds. Average FPS reports the second case
as "fine" even though the visual experience is awful.

**What SolarFlare does.** Tracks the last 10 seconds of frame
intervals (600 samples at 60 fps). Anything more than 1.5× the
expected period counts as a hitch. Stutter Score is `100 − 5 ×
(hitches / samples)`, so a single hitch over 10 s drops you 0.8
points; sustained 60+ hitches per 10 s drops you to zero.

Score colour-coding on the dashboard:
```
Stutter score: 94   ← green ≥90, blue ≥70, yellow ≥50, red <50
  6 hitches / 600 samples · worst 41ms
```

Code: `src/solarflare/stutter_score.{h,cpp}`.
API: `GET /api/solarflare/stutter-score?fps=60`.

## 3. Network Condition Heatmap

**The problem.** Wireless interference is bursty: a 200 ms spike in
RTT followed by 30 seconds of clean link. Average RTT hides this
completely. The network probe's aggregate score hides it. The
per-second loss counter shows it but it's not human-legible.

**What SolarFlare does.** Maintains a 60-second ring of bucketed
RTT-max and loss-count per target. The dashboard renders each
target's ring as 60 colour-coded cells: green when RTT ≤ 40 ms,
yellow ≤ 80 ms, orange > 80 ms, red when the bucket had packet
loss. Wireless bursts are immediately visible.

```
Network heatmap (last 60s):           score 87
■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■■
||||||  ← red cells here correspond to the Wi-Fi burst 30 s ago
```

Code: `src/solarflare/network_heatmap.{h,cpp}`.
API: `GET /api/solarflare/heatmap`.

## Why these three and not ten others

Every other "new feature" I could add to a streaming host is
incremental: more presets, more alerts, more knobs. The three above
show you something **you couldn't see before**:

- Predictive ABR **prevents** stutters, doesn't just respond to them.
- Stutter Score names the thing that actually matters (visible
  hitching) instead of the average it correlates with (FPS EWMA).
- Heatmap shows the bursty, transient nature of wireless problems
  that every other metric smooths away.

They're also independently useful: if you only care about #1,
the others don't cost you anything.

## How they fit together

```
   Moonlight client
        ↓
   ENet recv ──→  telemetry::record_latency(STAGE_NETWORK_RTT)
                              ↓
                     network_heatmap::submit_rtt
                              ↓
                     ┌────────┴────────┐
                     ↓                 ↓
              predictive_abr      network_probe
              (drop bitrate       (compute score)
               on jitter accel)
                     ↓
              config::video.max_bitrate ──→ encoder ──→ stream
                                                        ↓
                                  stutter_score::record (frame interval)
```

The three modules share one input (RTT samples) and produce three
complementary views: prevent stutter (#1), measure stutter (#2),
diagnose why (#3).
