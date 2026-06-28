# Codec Presets

Lumen ships **19 named quality presets** covering every encoder the
host runtime can negotiate with Sunshine.

A preset is a `(name, encoder, codec, description, kv)` tuple. The
`kv` is a list of `(config_key, value)` pairs; clicking "Apply" on a
preset would set those keys in the user's `sunshine.conf`.

## Presets by encoder

### NVENC (NVIDIA)

| Preset | Codec | Purpose |
|---|---|---|
| NVENC Low Latency 1080p60 | H.264 | P1, no B-frames, zerolatency |
| NVENC Balanced 1440p120 | HEVC | P4, 2 B-frames, lookahead |
| NVENC Quality 4K60 | HEVC | P7, 4 B-frames, lookahead 40, two-pass |
| NVENC AV1 4K120 | AV1 | P7 AV1 (Ada+ GPUs) |

### AMF (AMD)

| Preset | Codec | Purpose |
|---|---|---|
| AMF Low Latency 1080p60 | H.264 | speed, VBR, no preanalysis |
| AMF Balanced 1440p | HEVC | balanced, VBR, preanalysis on |
| AMF Quality 4K | AV1 | quality, lookahead on |

### VAAPI (Intel / AMD open-source)

| Preset | Codec | Purpose |
|---|---|---|
| VAAPI Low Latency | H.264 | CQP, strict RC buffer |
| VAAPI Balanced | HEVC | CQP, relaxed RC buffer |

### Software

| Preset | Codec | Purpose |
|---|---|---|
| Software x264 Low Latency | H.264 | x264 ultrafast, tune zerolatency |
| Software x264 Balanced | H.264 | x264 veryfast, tune fastdecode |
| Software x265 Quality | HEVC | x265 medium, no tune |
| Software SVT-AV1 6 | AV1 | SVT-AV1 preset 6 |

### VideoToolbox (macOS)

| Preset | Codec | Purpose |
|---|---|---|
| VideoToolbox Low Latency | H.264 | realtime on, sw encoders disallowed |
| VideoToolbox HEVC | HEVC | realtime on, sw encoders disallowed |

### QuickSync (Intel)

| Preset | Codec | Purpose |
|---|---|---|
| QuickSync Low Latency | H.264 | preset 6, CAVLC off |
| QuickSync HEVC Slow | HEVC | preset 3, slow_hevc on |

### Vulkan

| Preset | Codec | Purpose |
|---|---|---|
| Vulkan Low Latency | H.264 | rc_mode 2 (CBR), tune 2 (LL) |
| Vulkan Quality | HEVC | rc_mode 4 (VBR), tune 1 (HQ) |

## Where the presets live

- C++: `src/solarflare/codec_presets.{h,cpp}`
- API: `GET /api/solarflare/codec-presets`
- UI: `/codec-presets`

## Adding a preset

Append to `kPresets` in `codec_presets.cpp`. ponytail: ship the
minimum the user asked for; don't add "while we're here" presets.

```cpp
{"My Preset", "nvenc", "hevc", "What it does.",
 {{"nvenc_preset", "5"}, {"nvenc_rc", "vbr"}, {"qp", "21"}}}
```

The fields are the same as the upstream Sunshine config keys, so
the preset values can be copy-pasted straight into `sunshine.conf`.
