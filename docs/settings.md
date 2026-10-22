# Settings

The settings the host's web console shows are permanent surface: every client that can show them
shows them, the presets and the scripts read them, the docs explain them, and every later support
thread asks whether one was set. Add one only when it clearly makes streaming better for many
players.

The rules below follow punktfunk's `docs/settings.md` (MIT OR Apache-2.0, Copyright (c) 2026 unom
- Enrico Bühler), with Lumen's own settings named in the examples.

## Before adding a setting

Answer these in order. The first "yes" ends the search for a setting.

1. **Can it just work?** Detect, measure, or pick the right default. A setting is not a way to
   skip a decision.
2. **Does the platform already say it?** Follow the OS: display scale, text size, theme, reduced
   motion. Do not add an app copy of a system setting.
3. **Is it a fix for one device, one link, or one report?** Fix the detection, or put it in the
   log or in an operator environment variable. A per-device workaround is not a player setting.
4. **Can an existing control absorb it?** Reshape a control so it covers more players with no
   more choices, rather than adding a second one beside it.

A new setting also has to pass this: a player who does not know how streaming works reads the
label and its options and knows what to pick. If it needs a paragraph, it is not a player setting.

## Rules

- **Automatic means automatic.** No "Automatic, but…" modes and no tuning under an adaptive
  control. The bitrate is Automatic or a fixed rate; the parity follows the measured loss.
- **Never change what the player picked.** Resolution and refresh stay as chosen; adapting moves
  only what Automatic owns.
- **No hidden state across sessions.** Behaviour that silently depends on an earlier session is a
  setting the player cannot see.
- **Operator and diagnostic knobs are not player settings.** They are `LUMEN_*` environment
  variables or command-line flags.
- **One setting, every client that has the feature.** Shared key and wording. A setting that
  exists on one client only needs a reason that platform alone has.

## The settings Lumen has

| Setting | Meaning | Automatic? |
|---|---|---|
| Quality | The picture the host serves, in pixels. A client that cannot keep up asks for a smaller one; the operator may hold a size. | The client adapts it; the operator's choice is held. |
| Frame rate | The rate the host captures and encodes. | Fixed by the operator. |
| Rate control | Constant quantiser (qp) or constant bitrate (bits per second). | The bitrate moves with the link only when the adaptive bitrate is on. |
| Encoder | `--encoder vaapi` (default) or `--encoder nvenc`, a host start-up flag. | Fixed by the operator; a host without the chosen encoder refuses to start the stream. |
| Parity | How much forward error correction the host sends. | Follows the measured loss. |
| Audio | Whether new sessions carry audio. | Fixed by the operator. |
| Adaptive bitrate | Whether the bandwidth policy may move the bitrate on its own. | Off means the operator's bitrate is the bitrate. |

## Examples

Good — **Quality + Frame rate**. Two fields that answer "how big" and "how often", both of
which every session needs.

Bad — **Bitrate with a floor**. A third bitrate mode to understand: the client already asks for
a smaller picture when it cannot keep up, and the estimator already knows the loss.
