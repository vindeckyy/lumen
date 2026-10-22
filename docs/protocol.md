# Lumen wire protocol

Version 1. This document is normative; `crates/lumen-protocol` implements it.

## 1. Scope and transport

Lumen is LAN-only. Three logical channels exist:

| Channel | Transport | Reliability | Carries |
|---|---|---|---|
| Control | TCP, one connection, host listens | reliable, ordered | handshake, capability negotiation, pairing, NACKs, IDR requests, monitor list, clipboard, resolution changes |
| Video | UDP, host → client | unreliable, lossy, FEC-protected | encoded access units as slices |
| Audio | UDP, both directions | unreliable, FEC-protected | Opus or PCM frames |
| Input | UDP, client → host, separate socket | unreliable, low latency | input events |

Media never uses TCP. Head-of-line blocking on one lost media packet would stall video and input together, which is exactly the failure mode Lumen is designed to avoid.

Ports (host): control `24800/tcp`, video `24801/udp`, audio `24802/udp`, input `24803/udp`.

The media peer address is not negotiated. The host sends media to the IP address of the control connection's peer, on the client's media port, which is `24801/udp` by default. A host therefore serves one client media port at a time. See `docs/design.md`, decision D50.

## 2. Versioning and capability negotiation

1. The client opens the control connection and sends `Hello { version, capabilities: u64, device_key: [u8;32] }`.
2. The host replies `HelloAck { version, capabilities: u64, session_id: u32, paired: bool, host_key: [u8; 32], confirm: [u8; 32], signature: [u8; 64] }`. The host key is the host's Ed25519 public key, and the signature proves the host owns it on this connection, so the client can pin the host it paired with. See section 6.
3. The negotiated version is `min(client.version, host.version)` and must be in the list of versions both builds support.
4. The negotiated capability set is the bitwise AND of the two bitmaps. A session whose intersection is empty is rejected.
5. Unknown capability bits are ignored on receive (forward compatibility) but rejected by `Capabilities::from_bits_strict` when a build explicitly validates a bitmap.

Capability bits (bit 0 is least significant):

| Bit | Name | Meaning |
|---|---|---|
| 0 | `H264` | H.264 video |
| 1 | `HEVC` | HEVC video |
| 2 | `AV1` | AV1 video |
| 3 | `CHROMA_444` | 4:4:4 chroma |
| 4 | `BIT_DEPTH_10` | 10-bit pixels |
| 5 | `HDR` | HDR metadata transport |
| 6 | `FEC_RS` | Reed-Solomon FEC in addition to XOR parity |
| 7 | `REQUEST_IDR` | Client can request an on-demand IDR |
| 8 | `RELATIVE_POINTER` | Relative pointer mode |
| 9 | `POINTER_CONSTRAINTS` | Pointer constraints |
| 10 | `TOUCH` | Touch input |
| 11 | `PEN` | Pen input |
| 12 | `GAMEPAD` | Gamepads |
| 13 | `AUDIO_BIDIRECTIONAL` | Bidirectional audio |
| 14 | `CLIPBOARD` | Clipboard transfer |
| 15 | `MULTI_MONITOR` | Multiple monitors |
| 16 | `RESOLUTION_CHANGE` | Arbitrary resolution changes mid-session |
| 17 | `RAW_CODEC` | Raw uncompressed codec (correctness tests only) |
| 18 | `SHADOW_MODE` | View-only shadow session |

## 3. Media packet format

Every media packet starts with a fixed 42-byte header, followed by the payload. The header is included in the AEAD associated data, so it is authenticated.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     'L'      |     'M'      |    version    |     kind      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|            flags             |           slice_index         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|          slice_count         |           frame_id           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                       timestamp_us (u64)                      +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|            aux               |            length             |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         au_len (u32)                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        session_id (u32)                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                        seq (u64)                              +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Offset | Size | Field | Type | Meaning |
|---|---|---|---|---|
| 0 | 2 | magic | `[u8; 2]` | `0x4C 0x4D` (`"LM"`) |
| 2 | 1 | version | u8 | Protocol version, currently `1` |
| 3 | 1 | kind | u8 | Message kind (see below) |
| 4 | 2 | flags | u16 | Kind-specific flags. For a media slice it carries the number of data slices per FEC block, so the receiver can map a slice index to its block. Zero means eight |
| 6 | 2 | slice_index | u16 | Index of this slice inside the access unit. For `VideoParity` this is the parity index within the block. |
| 8 | 2 | slice_count | u16 | Total slices in the access unit |
| 10 | 4 | frame_id | u32 | Monotonic frame identifier, host-assigned |
| 14 | 8 | timestamp_us | u64 | Host capture timestamp of the frame, microseconds, host clock |
| 22 | 2 | aux | u16 | Kind-specific auxiliary word (FEC block index) |
| 24 | 2 | length | u16 | Payload length in bytes after the header |
| 26 | 4 | au_len | u32 | True length of the access unit, before slice padding |
| 30 | 4 | session_id | u32 | Incremented on every reconnect; stale packets are dropped |
| 34 | 8 | seq | u64 | Per-packet sequence number. It is the AEAD nonce counter, so it never repeats within a session. A retransmission carries a new value. |

All multi-byte fields are little-endian.

`seq` is sent in the clear because the receiver needs it to open the packet, but it is covered by the AEAD associated data, so a peer cannot change it without invalidating the tag. `session_id` is likewise authenticated.

### 3.1 Message kinds

| Value | Kind | Direction | Meaning |
|---|---|---|---|
| 1 | `VideoData` | host → client | One slice of a video access unit |
| 2 | `VideoParity` | host → client | One parity slice protecting a FEC block |
| 3 | `Audio` | both | One audio frame |
| 4 | `Input` | client → host | One input event |
| 5 | `Control` | both | Control message on the reliable channel |

### 3.2 Access unit slicing

An encoded access unit is split into slices of at most 1200 payload bytes. Slice `i` covers bytes `[i*1200, min((i+1)*1200, len))`. The decoder starts decoding when the last required slice of an access unit has arrived. Slices arrive out of order; the receiver indexes them by `slice_index`.

`slice_count` in the header always equals the number of slices in the access unit, so the receiver can size its reassembly buffer from the first slice it sees.

### 3.3 The audio frame

An audio frame is a fixed 48-byte header followed by the encoded payload.
The header is sent in the clear and is covered by the AEAD associated data,
so a peer cannot change it without invalidating the tag.

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 2 | magic | `LA` |
| 2 | 1 | version | `1` |
| 3 | 1 | kind | 1 Opus, 2 PCM |
| 4 | 2 | channels | Interleaved channels |
| 6 | 2 | reserved | Zero |
| 8 | 4 | sample_rate | Hertz |
| 12 | 4 | frame_duration_us | 2500, 5000 or 10000 |
| 16 | 8 | frame_index | The host's frame counter |
| 24 | 8 | video_capture_ts_us | The host capture time of the audio |
| 32 | 4 | payload_len | Bytes of payload that follow |
| 36 | 4 | reserved | Zero |
| 40 | 8 | seq | The AEAD counter, in the clear |

The `seq` field is the AEAD counter of the packet. It is in the clear
because the receiver needs it to open the payload, and it is authenticated as
associated data.

`video_capture_ts_us` is the host's capture time of the audio itself. The
client uses it to find the video frame of the same moment: it keeps the
video frames it has presented and picks the one whose capture timestamp is
closest. The audio is then played when that video frame was on screen.

### 3.4 Frame timing

`timestamp_us` is the host capture timestamp taken from the capture source (PipeWire `spa_meta_header`, or the equivalent in other capture paths). It is the anchor for every latency measurement: the client maps it onto its own clock using the offset estimated over the control channel, then reports

```
pipeline_latency = client_present_time - (capture_timestamp + clock_offset)
```

## 4. FEC

Video access units are grouped into FEC blocks of 8 data slices (12 at the heavy level). The parity slices of a block are the Reed-Solomon recovery shards over GF(2^16) (Leopard-RS) that the `reed-solomon-simd` 3.x crate computes for `original_count` = the block's data-slice count and `recovery_count` = its parity count: 2 for a block of 8, 3 for a block of 12. Data slice `i` of the block is original shard `i`, parity slice `p` is recovery shard `p`. As many losses inside one block as it has parity slices are recoverable from parity alone; any further loss is recovered by retransmission or dropped.

Parity slices are sent as `VideoParity` packets. `aux` carries the block index; the parity slice index within the block is carried in `slice_index`.

A block whose last data slice is short is padded with zero slices before the parity is computed, so the receiver can rebuild the same padded block.

Parity ratio adapts to measured loss: no parity below 0.5 % measured loss, 2 per 8 below 2 %, 3 per 12 at or above 2 %. Total overhead is capped at 25 %. The block size travels in the `flags` word. See `docs/design.md`, decisions D59, D60 and D82.

## 5. Retransmission

A receiver that misses a slice sends a NACK on the control channel listing the missing `(frame_id, slice_index)` pairs. The host retransmits only while

```
now < frame_capture_timestamp + 1.5 * frame_interval
```

A frame that misses its deadline is dropped; the client then requests an on-demand IDR if the codec supports it. The pipeline is never blocked waiting for a late packet.

**Not implemented yet.** The host answers a `Nack` by resealing the slice, but the client does not yet send one: it drops an access unit it cannot reassemble and asks for a keyframe instead. The retransmit deadline is not enforced against the frame's capture timestamp yet. See `docs/design.md` and `docs/known-limits.md`.

## 6. Cryptography

Handshake: Noise `XX` pattern, X25519, ChaCha20-Poly1305 for the handshake itself. The transport AEAD is AES-256-GCM, chosen because the target hardware has AES-NI and PCLMULQDQ.

Key separation: the Noise handshake produces one shared secret. HKDF-SHA256 derives four independent keys, one per channel and direction, each with a distinct info string:

| Domain | Info string | Use |
|---|---|---|
| control host→client | `lumen-control-h2c-v1` | Control records sent by the host |
| control client→host | `lumen-control-c2h-v1` | Control records sent by the client |
| media host→client | `lumen-media-h2c-v1` | Video packets |
| media client→host | `lumen-media-c2h-v1` | Input packets |
| audio host→client | `lumen-audio-h2c-v1` | Audio packets |
| audio client→host | `lumen-audio-c2h-v1` | Audio packets the client sends back |

The audio has a key of its own because it has a sender of its own: the video
sender and the audio sender would otherwise use the same key and the same
counter, which is a nonce reuse.

Each key is used by exactly one sender, so the two directions of a channel never share a (key, nonce) stream.

Nonces are 12 bytes: the 8-byte little-endian per-direction packet counter, then the 4-byte little-endian session id. Each direction starts its counter at zero under its own key, and the counter never repeats under one key.

Pairing: the host stores an Ed25519 identity key. The client owns its own Ed25519 identity key. Every session begins with a pairing exchange on the control channel. Everything in it is bound to the connection: the binding `B` is the client's Noise static public key followed by the host's, as each end saw them in the handshake. A third party in the middle runs one handshake with each end, with its own static keys, so the two ends then hold different `B`.

1. The client sends `Hello { version, capabilities, device_key }`, where `device_key` is the client's Ed25519 public key.
2. The host answers `PairingChallenge { host_key, nonce, pake }`. `nonce` is 32 fresh random bytes. When the host does not trust `device_key` and the pairing mode is `prompt`, the host draws a fresh random six digit code, shows it (log, `--pairing-code-file` with owner-only permissions, control plane), and `pake` is its SPAKE2 message over the code (Ed25519 group, side B, 33 bytes). The SPAKE2 identities are the client's and the host's Noise static keys and the password is the code as a little-endian `u32`. Otherwise `pake` is all zero and no code is asked for.
3. The client signs `b"lumen-pairing-proof-v2" || nonce || host_key || device_key || B` with its Ed25519 key. When `pake` is not zero, the user types the code on the client; the client runs SPAKE2 side A with it and derives two confirmations from the shared key `K`: `HKDF-SHA256(K, info = label || B)` with the labels `lumen-pair-client` and `lumen-pair-host`. It answers `PairingResponse { signature, pake, confirm }` with its own SPAKE2 message and the client confirmation, or zeros when no code was asked for or typed.
4. The host verifies the signature against `device_key`. A client that cannot prove it owns the key is refused.
5. If the host trusts `device_key`, the client is authorised. Otherwise, in `prompt` mode, the host finishes SPAKE2 with the client's message and compares the client confirmation in constant time. A match pairs the client, and the host adds its public key to the trust store. A wrong code, a missing one, or a third party in the middle gives a different key, so the confirmations disagree and the client is refused. In `refuse` mode an untrusted client is refused.
6. The host answers `HelloAck`. `signature` is the host's Ed25519 signature over `b"lumen-host-proof-v1" || host_key || device_key || B`; `confirm` is the host confirmation when this session paired the client, zero otherwise.
7. The client verifies the host signature against `host_key`, and, when it typed a code, the host confirmation. Either failing ends the session.

The code never crosses the wire and is fresh for each connection. A relayed code is worth nothing, because of `B`, and an online guess is worth one in a million per connection; the host does not limit the rate of attempts (see `known-limits.md`). The trust store holds the host's private key and the public keys of the clients it trusts, in one file with owner-only permissions.

The client pins the host: it keeps the host key next to its identity and refuses a session with a different host key. The first pin is only written after the host proved, in the same session, the code the user typed. A client with no pin that meets a host which trusts it already (its pin file was lost) is refused, and the host's operator removes the client from the trusted clients so it can pair again. This is the SPAKE2 pairing of punktfunk, with Noise static keys in the place of its certificate fingerprints.

The control channel is encrypted from the first byte.

## 7. Control channel framing

The connection carries three kinds of frame, in this order.

**Noise handshake frames.** The session starts with a Noise `XX` handshake run directly on the TCP connection, before any control record exists. Each handshake message is a 16-bit big-endian length prefix followed by the message. The client is the initiator and sends the first message. After the handshake both ends hold the four session keys of section 6.

**Control records.** Each record is a `u32` little-endian body length, then the body. The body is the AES-256-GCM ciphertext of the encoded message, tag included, with empty associated data. Each direction has its own key and a counter starting at 0; the receiver counts the records it opens. A body over 1 MiB is rejected before allocation. After any error the channel is dropped.

**The pairing exchange.** The records `Hello`, `PairingChallenge`, `PairingResponse` and `HelloAck` come in that order, before any other message. See section 6.

**The clock negotiation.** The capability bitmap travels inside the Noise handshake: the initiator puts it in the payload of its first handshake message, the responder in the payload of its second. The payload is part of the handshake transcript, so a peer that changes it fails the final hash check. The bitmap settles the transport cipher before the first control record exists, which keeps the control channel encrypted from its first byte. A peer without AES-NI sets the `AEAD_CHACHA20` bit and the session uses ChaCha20-Poly1305 instead of AES-256-GCM.

**The stage timing exchange.** The host sends `Ping` every 500 ms and the client answers with `Pong`. From the host's send timestamp, the client's reply timestamp and the host's receive timestamp, the host estimates the offset between the two `CLOCK_MONOTONIC` clocks. The client then reports `StageTimes` for every frame it presents. The two exchanges let the host compute a per-stage latency for the whole pipeline, not just for the stages on its own side.

A message body is one kind byte followed by its fields in the order below. Integers are little-endian, with no padding.

| Kind | Message | Fields |
|---|---|---|
| 1 | `Hello` | version `u8`, capabilities `u64`, device key `[u8; 32]` |
| 2 | `HelloAck` | version `u8`, capabilities `u64`, session id `u32`, paired `u8` (0 or 1), host key `32` bytes, confirmation `32` bytes, signature `64` bytes |
| 3 | `RequestIdr` | frame id `u32` |
| 4 | `Nack` | frame id `u32`, count `u16`, then `count` slice indices `u16` |
| 5 | `Monitor` | index, width, height, refresh Hz, each `u32` |
| 6 | `ResolutionChanged` | width `u32`, height `u32` |
| 7 | `Clipboard` | length `u32`, UTF-8 bytes |
| 8 | `Ping` | timestamp in microseconds `u64` |
| 9 | `Pong` | timestamp in microseconds `u64`, reply timestamp in microseconds `u64` |
| 10 | `Bye` | none |
| 11 | `PairingChallenge` | host key `[u8; 32]`, nonce `[u8; 32]`, SPAKE2 message `[u8; 33]` |
| 12 | `PairingResponse` | signature `[u8; 64]`, SPAKE2 message `[u8; 33]`, confirmation `[u8; 32]` |
| 14 | `LinkFeedback` | slices received `u64`, slices lost `u64`, round trip time `u64` microseconds |
| 13 | `StageTimes` | frame id `u32`, recv `u64`, decode start `u64`, decode `u64`, present `u64` |
| 16 | `Recover` | last good frame id `u32` |
| 17 | `Cursor` | serial `u32`, hotspot x `u16`, hotspot y `u16`, width `u16`, height `u16`, then `width * height` premultiplied ARGB pixels, each a little-endian `u32` |
| 18 | `Rumble` | pad `u8`, strong motor `u8`, weak motor `u8` |

`Recover` is how the client answers a frame it cannot decode because its reference was lost. It names the newest frame it decoded. When the encoder still holds that frame as a reference (up to eight frames, fewer at large sizes), its next P picture predicts from it instead of from the lost frames, and the host puts a recovery point SEI (payload type 6, `recovery_frame_cnt` 0, `exact_match_flag` 1) before its first slice. Every picture encoded since the named frame is distrusted and never anchors a later recovery. The SPS sets `gaps_in_frame_num_value_allowed_flag`, and the client infers the lost `frame_num`s as non-existing frames (H.264 8.2.5.2), which keeps the reference windows of both ends equal. The client refuses frames until an IDR or a recovery point arrives. A host that no longer holds the frame sends an IDR. A client that has decoded nothing yet sends `RequestIdr`, and a client whose request went unanswered for 250 ms asks for an IDR.

`Cursor` carries the host's cursor shape when the capture reports it apart from the frames, which is the X11 capture today. The host sends a shape once per serial. The client shows it as its own window cursor, so the pointer moves without waiting for the network. A width of 0 hides the cursor.

`Rumble` is the host telling the client what the game asked of a virtual pad's motors: the strong (low-frequency) and the weak (high-frequency) motor, 0 to 255, for the pad with that index. The client plays them on the pad of its own machine that drives that slot. The host sends a level when it changes and renews a held level every 250 ms, so the client keeps playing it and notices a link that died. A level of zero for both motors stops the rumble.

`LinkFeedback` is the client telling the host what the link did. The counts are cumulative, so the host differences them against the previous report. The host's bandwidth and parity policies read it: the loss decides the parity level, the round trip time and the loss decide the target bitrate.

`StageTimes` carries the client's timestamps for one presented frame. The host maps them onto its own clock with the offset it estimates from the ping exchange, so it can measure the whole pipeline. The timestamps are on the client's `CLOCK_MONOTONIC`.

Capability bits unknown to this build are dropped on decode, so a newer peer can talk to an older one.


## 8. Input events

Input events travel on their own UDP socket, so a lost video packet cannot delay them. Each event is one datagram. The host binds UDP port 24803 by default, configurable. Nothing else shares this socket, and events are never batched or split across datagrams.

### 8.1 Datagram

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | sequence, u64 LE, in the clear. This is the AEAD associated data. |
| 8 | n | AEAD ciphertext of the plaintext below |
| 8 + n | 16 | AEAD tag |

The datagram is sealed with the client-to-host media key of section 6. The plaintext is:

| Field | Size | Meaning |
|---|---|---|
| kind | 1 | event kind, see 8.2 |
| client_ts_us | 8, u64 LE | `CLOCK_MONOTONIC` microseconds on the client, read immediately before sealing |
| sequence | 8, u64 LE | the same value as the clear header |
| fields | per kind | see 8.2 |

`client_ts_us` and `sequence` together form the 16-byte client stamp that follows the kind byte. The receiver rejects a datagram whose stamp sequence differs from its header.

Sequence numbers start at 0 in each session and advance by one for each event sealed. The AEAD nonce is derived from the same counter, as section 6 specifies. A sequence number that never arrives is a loss.

### 8.2 Event kinds

Integers are little-endian. A boolean is one byte, 0 or 1. Coordinates and deltas are IEEE 754 binary64, little-endian, and must be finite.

| Kind | Event | Fields |
|---|---|---|
| 1 | Key | keycode u32 (evdev code), pressed bool |
| 2 | Motion, relative | dx f64, dy f64 (logical pixels) |
| 3 | Motion, absolute | x f64, y f64 (client-surface pixels) |
| 4 | Button | button u32 (evdev code), pressed bool |
| 5 | Scroll | dx f64, dy f64 (logical pixels) |
| 6 | Gamepad | pad u8 (0 to 3), buttons u16, lx i16, ly i16, rx i16, ry i16, lt u8, rt u8 |
| 7 | Gamepad removed | pad u8 (0 to 3) |
| 8 | Touch | slot u8 (0 to 9), phase u8 (0 down, 1 motion, 2 up), x f64, y f64 |
| 9 | Pen | x f64, y f64, pressure u16 (0 to 4095), in range bool |

A `Gamepad` event is the whole state of one pad, sent on every change and again every 100 ms, so a lost datagram costs one repeat at most. Buttons are XInput bits: d-pad up `0x0001`, down `0x0002`, left `0x0004`, right `0x0008`, start `0x0010`, back `0x0020`, left stick `0x0040`, right stick `0x0080`, left shoulder `0x0100`, right shoulder `0x0200`, guide `0x0400`, A (bottom) `0x1000`, B (right) `0x2000`, X (left) `0x4000`, Y (top) `0x8000`. Sticks run from -32768 to 32767 with right and up positive; triggers run from 0 to 255. A pad index of 4 or more is malformed. The host gives each pad index its own virtual DualSense and removes it on `Gamepad removed` or when the session ends.

Touch and pen coordinates are a fraction of the client's screen, from 0 (left or top edge) to 1 (right or bottom edge), so a contact lands on the same spot of the capture whatever picture size the client is shown. The host injects them into a virtual touchscreen and a virtual pen made through uinput, with axes the size of the capture. A slot of 10 or more, an unknown phase or a non-finite coordinate is malformed.

### 8.3 Receiver rules

The receiver checks each datagram in this order and drops it at the first failure:

1. Longer than 128 bytes: dropped as oversize. It is not decrypted.
2. Shorter than 24 bytes (header plus tag): dropped as truncated.
3. Tag does not verify: dropped and counted as an authentication failure.
4. Plaintext malformed: unknown kind, truncated, trailing bytes, boolean not 0 or 1, non-finite float, or stamp sequence differing from the header. Dropped and counted as malformed.
5. Replay: a sequence already accepted, or 64 or more behind the newest accepted, is dropped and counted. The window only moves after steps 3 and 4 pass, so forged datagrams cannot move it. Reordering within the window is accepted.
6. The event is injected before the next datagram is read. No queue sits between the socket and the injector, so input never waits behind other input. Gamepad events go to the virtual pads instead of the keyboard and pointer injector.

The AEAD and the replay window give integrity and freshness within a session. Client identity comes from pairing in section 6, not from this layer.

## 9. Audio

Audio frames carry the host capture timestamp of the video frame they align to, so the client can align playback with the video clock. Opus frames are 2.5, 5 or 10 ms at 48 kHz; PCM mode is also defined.

## 10. Version history

| Version | Change |
|---|---|
| 1 | Initial protocol. |
