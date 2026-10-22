# Agent A notes — M3 input path

This file records what the lead has to wire in, because the agent that wrote
the M3 input modules was not allowed to touch the `lib.rs` files or the root
`Cargo.toml`.

Files written by this agent:

| File | What it is |
|---|---|
| `crates/lumen-input-x11/src/lib.rs` | the public API of the X11 capture crate |
| `crates/lumen-input-x11/src/raw.rs` | XI2 raw event capture |
| `crates/lumen-input-x11/src/keyboard.rs` | keycode, keysym and evdev translation |
| `crates/lumen-input-x11/tests/env_display.rs` | the integration test, skips without `DISPLAY` |
| `crates/lumen-input-x11/Cargo.toml` | the dependencies of the crate |
| `crates/lumen-transport/src/input_channel.rs` | the isolated input socket and thread |
| `crates/lumen-bench/src/input_latency.rs` | the input latency measurement |
| `scripts/run-input-latency.sh` | the measurement script |

---

## Wiring needed

### 1. `crates/lumen-transport/src/lib.rs`

Add the module and re-export its types:

```rust
pub mod input_channel;
```

```rust
pub use input_channel::{
    InputChannelError, InputChannelStats, InputReceiver, InputSender, InputThread,
    InputThreadOutcome,
};
```

No new dependency is needed: `input_channel.rs` uses `lumen-crypto`,
`lumen-protocol` and `socket2`, which the crate already has.

The module was written so it can be checked without being registered: it only
reaches into `crate::socket::SocketOptions`, which the crate already has.

### 2. `crates/lumen-bench/src/lib.rs`

Add the module and re-export its types:

```rust
pub mod input_latency;
```

```rust
pub use input_latency::{InputLatencyRecorder, STAGE_NAME as INPUT_LATENCY_STAGE};
```

No new dependency is needed: the module uses `crate::record::Stats` and
`crate::report::RunReport`, which the crate already has.

### 3. `crates/lumen-input-x11/Cargo.toml`

This file is inside the crate this agent owns, so the entries are already
applied:

```toml
[dependencies]
lumen-protocol = { workspace = true }
x11rb = { workspace = true }
xkbcommon = { workspace = true }
rustix = { workspace = true }
thiserror = { workspace = true }
```

`x11rb` and `xkbcommon` are already in the workspace dependency list, and both
are used with `{ workspace = true }`. `rustix` and `thiserror` are in the
workspace list as well.

Two things the lead should know about the `xkbcommon` entry:

* The workspace entry is `xkbcommon = { version = "0.9", features = ["wayland",
  "x11"] }`, so the `x11` feature is on. That feature links
  `libxkbcommon-x11`, so a build machine needs `libxkbcommon-x11-dev`. This
  machine has it. The CI workflow installs neither `libxkbcommon-dev` nor
  `libxkbcommon-x11-dev`, and `lumen-input-wayland` already needs
  `libxkbcommon-dev`, so the CI package list is worth a look.
* The `x11` feature turns on `as-raw-xcb-connection`, so **`Cargo.lock` was
  updated by cargo**: the `xkbcommon` entry now lists
  `as-raw-xcb-connection`. The package was already in the lock file, only the
  dependency edge of `xkbcommon` is new.

### 4. Nothing else

The `lumen-input-x11` crate is already a member of the workspace, so no
`Cargo.toml` at the root has to change.

---

## Notes for the review

### The X11 raw events

Three things have to hold for the XI2 raw events to arrive, and all three
were found by experiment on an Xvfb display, not by reading the protocol:

1. `XIQueryVersion(2, 0)` has to be sent before any XI2 request. Without it
   the server answers `BadValue`.
2. `XISelectEvents` takes **one** 32-bit mask per device, with every event
   type in that one word. A list of masks is refused with `BadValue`
   (`bad_value` 45). `x11rb` models the mask as a `Vec<XIEventMask>`, so the
   code ORs the bits into one element.
3. The events are selected for the **master** devices (`deviceid` 1). With
   `XIAllDevices` every event arrives once per device in the chain, so a key
   press arrives twice.

A raw motion event of a relative device carries the **movement itself** in
`axisvalues`, not the accumulated position. The mask of the event is a list
whose first element is the bitmask of the present valuators; the number of
values that follow is the number of set bits in it.

The axis of a valuator is named by the label of the device, which the server
gives as an atom. On a normal pointer the labels are `Rel X` and `Rel Y`, and
the master pointer reports its valuators in the mode `RELATIVE`.

### The measured latency

`crates/lumen-input-inject/examples/input_latency.rs` (which already existed)
stamps an event **when it seals it**, so the number it reports is the cost of
the transport and the injection. The sampling period of the client is outside
the measurement already. The script records that period in the report as
`client_polling_interval_us` so the report says so out loud.

`crates/lumen-bench/src/input_latency.rs` is the general form of the same
measurement: it takes the two stamps of an event, maps the client stamp onto
the host clock with the NTP-style offset of
`crates/lumen-host-service/src/stages.rs`, and subtracts the polling interval
of the client, which is an explicit input.

### The build directory

`/tmp` was nearly full (7.6 GiB of tmpfs, 6.7 GiB used by other agents), so
this agent built into `/home/hayden/.cache/lumen-agent-a-target` instead of
`/tmp/opencode/target-agent-a`. The isolation is the same: no other agent
writes there.

### The measured result

`scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json`
ran over the namespace pair on this machine:

| | |
|---|---|
| events | 60000, at 1000 per second, 60 s |
| injected | 60000, no failures, no losses, no replays |
| p50 | 41 us |
| p90 | 85 us |
| p99 | 193 us |
| max | 4079 us |
| mean | 50.9 us |

The host load was 2.56 / 3.79 / 3.70 before the run and 2.61 / 3.55 / 3.62
after it, so the machine was not busy. The p99 is under the millisecond the
plan asks for, but the number is provisional: it is the software leg between
the client stamp and the host injection call, over a veth pair, with no
display, no compositor and no real client in it.

### Failures that are not this agent's

While this agent was working, other agents were editing
`crates/lumen-transport` and `crates/lumen-client-desktop`. At the time of
writing:

* `cargo clippy --workspace --all-targets -- -D warnings` fails with three
  findings in `crates/lumen-transport/src/fec.rs` (the Vandermonde solver)
  and `crates/lumen-transport/tests/properties.rs`. The files were modified
  at 15:19 and later, so they are a work in progress.
* `cargo test -p lumen-transport` fails in `slice.rs` and `media.rs` (four
  FEC tests).
* `crates/lumen-client-desktop` does not compile at the moment
  (`reconnector` is not declared mutable).

None of them is in a file this agent wrote. The three crates this agent owns
or added are green: `lumen-input-x11` (24 unit tests, 1 integration test, 1
doctest), and the two new modules (6 tests in `input_channel.rs` and 13 in
`input_latency.rs`) once they are registered.

