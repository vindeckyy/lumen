/**
 * @file src/solarflare/stutter_score.h
 * @brief SolarFlare Stutter Score: frame interval histogram + a
 *        single quality number.
 *
 * ponytail: what users actually care about isn't FPS or jitter in
 * isolation -- it's "how often did a frame take noticeably longer
 * than it should have". A 60 fps stream that delivers 59.5 fps
 * average feels great; a 60 fps stream that delivers 60 fps
 * average but with one 80 ms hitch every 3 seconds feels terrible.
 * Average FPS reports the first case; Stutter Score exposes the
 * second.
 *
 * Algorithm:
 *
 *   - Keep a ring buffer of the last N (default 600) inter-frame
 *     intervals.
 *   - For each frame, `expected = fps_period_us` (1_000_000 / fps).
 *   - Count a "hitch" when interval > expected * 1.5 (50% slow).
 *   - Stutter Score = (1 - hitches / N) * 100, clamped to [0, 100].
 *
 * The user sees a 0-100 score on the dashboard and a sparkline of
 * intervals. The number drops sharply on the first hitch, even if
 * FPS average is fine -- which is the whole point.
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>
#include <deque>
#include <mutex>

namespace solarflare {

  struct stutter_snap_t {
    double score = 100;      ///< 0-100
    std::uint64_t hitches = 0;
    std::uint64_t samples = 0;
    double avg_interval_us = 0;
    double max_interval_us = 0;
    std::uint32_t fps = 0;   ///< FPS the score is computed against
  };

  namespace stutter_score {
    /** @brief Record a frame-pacing sample (interval in microseconds). */
    void record(std::uint32_t interval_us);

    /** @brief Compute score against a given FPS (e.g. negotiated fps). */
    stutter_snap_t snapshot(std::uint32_t fps = 60);

    /** @brief Reset the ring buffer. */
    void reset();
  }  // namespace stutter_score
}  // namespace solarflare
