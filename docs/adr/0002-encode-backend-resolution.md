# 2. The encoder backend and the resolution

Date: 2026-10-22

## Status

Accepted.

## Context

The host encodes H.264. VA-API covers Intel and AMD; NVENC covers NVIDIA, where VA-API is
decode-only. The picture the client asks for is often smaller than the picture the compositor
has, so the pipeline resizes somewhere.

## Decision

The encoder is one trait, `VideoEncoder`, with one implementation per backend. The backend is
chosen by what the machine offers, not by a setting:

- `lumen-encode-vaapi` for a DRM render node with a VA-API encoder entry point.
- `lumen-encode-nvenc` and `lumen-encode-vulkan` are stubs today; when they exist they are
  chosen the same way, by probing the device.

The resolution is decided in one place, in this order:

1. The picture the client asked for, if it is smaller than the capture.
2. The size the compositor reports, when the client asked for nothing.
3. The size the encoder can do, which the backend reports when it is opened.

The conversion happens on the GPU when the capture's render node has a video processor, and on
the CPU otherwise. The encoder receives NV12 in either case, so the backend does not care how the
picture was made.

## Consequences

- The client can ask for a smaller picture and the host honours it without a reconfigure: the
  quality ladder asks for the next size down and the capture stage scales.
- The encoder never sees a resolution it cannot hold, because the choice is made before it is
  opened.
- A new backend is a new crate and one arm of the probe. The wire format does not change.

## Alternatives considered

- **Let the encoder scale.** An encoder that scales has to be asked twice: once for the size and
  once for the rate. The picture would be scaled twice on the way.
- **One encoder crate with a backend enum.** The same argument as in the capture contract: the
  backends fail differently and share little.
