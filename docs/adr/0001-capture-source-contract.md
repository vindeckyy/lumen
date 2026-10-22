# 1. The capture source contract

Date: 2026-10-22

## Status

Accepted.

## Context

The host captures a compositor, an X11 display, or a portal stream. Each of them fails in its own
way, and each of them has its own idea of what a frame is: a wlroots screencopy buffer, a DRI3
pixmap, a PipeWire buffer. The pipeline downstream should not care.

## Decision

A capture source is one type with three jobs:

- **Give frames.** The source produces `VideoFrame`s: NV12 in host memory or a DMA-BUF, with a
  capture timestamp and a frame index.
- **Say what went wrong.** A failure is an error the source reports, not a panic and not a silent
  stop. The capture stage decides what to do with it.
- **Report its output.** The source names the output it captures and its size, so the host can
  send the monitor list to the client and notice a mode change.

The host passes facts to the source, never capture choices: the source is told which output to
capture and at what rate, and it decides how.

The sources are `lumen-capture-wlr` (wlroots screencopy), `lumen-capture-x11` (DRI3, MIT-SHM,
GetImage) and `lumen-capture-pipewire` (the portal, still a stub).

## Consequences

- A new capture source is one crate that implements the contract; nothing else changes.
- The capture stage reopens the source when it fails, which is what makes a compositor restart
  survivable. The source's socket name may change across a restart, so the wlroots source falls
  back to the sockets in `$XDG_RUNTIME_DIR`.
- The GPU conversion and the encoder see one frame type, so the zero-copy path is a property of
  the frame, not of the source.

## Alternatives considered

- **One capture crate with a backend enum.** A single enum grows a variant per compositor and
  every variant's state ends up in the same type. Separate crates keep the failure modes apart.
- **Let the source decide when to give up.** The host owns the session, so it owns the retry
  policy and the backoff.
