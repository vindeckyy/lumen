/**
 * @file src/solarflare/stutter_score.cpp
 * @brief Definitions for the stutter score module.
 */

// standard includes
#include <algorithm>
#include <chrono>
#include <mutex>

// local includes
#include "solarflare/stutter_score.h"

namespace solarflare {

  namespace stutter_score {

    namespace {
      constexpr std::size_t kRingCapacity = 600;  // 10 s at 60 fps
      std::mutex g_mtx;
      std::deque<std::uint32_t> g_intervals;
      std::chrono::steady_clock::time_point g_last = std::chrono::steady_clock::now();
    }  // namespace

    void record(std::uint32_t interval_us) {
      std::lock_guard<std::mutex> lock(g_mtx);
      if (g_intervals.size() >= kRingCapacity) g_intervals.pop_front();
      g_intervals.push_back(interval_us);
    }

    void reset() {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_intervals.clear();
      g_last = std::chrono::steady_clock::now();
    }

    stutter_snap_t snapshot(std::uint32_t fps) {
      stutter_snap_t out;
      out.fps = fps;
      if (fps == 0) fps = 60;  // sane default
      const std::uint32_t expected = static_cast<std::uint32_t>(1000000 / fps);

      std::lock_guard<std::mutex> lock(g_mtx);
      out.samples = g_intervals.size();
      if (g_intervals.empty()) return out;

      std::uint64_t sum = 0;
      std::uint32_t max_us = 0;
      std::uint64_t hitches = 0;
      for (auto us : g_intervals) {
        sum += us;
        if (us > max_us) max_us = us;
        if (us > expected + expected / 2) ++hitches;  // > 1.5x expected
      }
      out.hitches = hitches;
      out.avg_interval_us = static_cast<double>(sum) / g_intervals.size();
      out.max_interval_us = max_us;
      double pct = static_cast<double>(hitches) / g_intervals.size() * 100.0;
      out.score = std::max(0.0, std::min(100.0, 100.0 - pct * 5.0));
      // 5x weight makes a few hitches drop the score noticeably but
      // doesn't tank on the occasional one-off.
      return out;
    }
  }  // namespace stutter_score
}  // namespace solarflare
