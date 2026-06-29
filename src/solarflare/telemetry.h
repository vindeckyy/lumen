/**
 * @file src/solarflare/telemetry.h
 * @brief SolarFlare telemetry: per-stage latency + throughput counters.
 *
 * ponytail: stat_trackers::min_max_avg_tracker exists upstream but
 * only fires its callback on an interval, so it can't give a live
 * snapshot. The honest move is the 6-line running min/max/avg we
 * actually need. No reservoir, no publisher thread -- snapshot()
 * reads atomics and one tiny mutex.
 */
#pragma once

// standard includes
#include <atomic>
#include <chrono>
#include <cstdint>
#include <functional>
#include <limits>
#include <mutex>
#include <string>

namespace solarflare {

  // ponytail: enum order is array index, keep stable.
  enum pipeline_stage : std::size_t {
    STAGE_INPUT = 0,
    STAGE_CAPTURE,
    STAGE_COLOR_CONVERT,
    STAGE_ENCODE,
    STAGE_PACER,
    STAGE_SEND,
    STAGE_NETWORK_RTT,
    STAGE_COUNT
  };

  inline constexpr std::size_t stage_count = STAGE_COUNT;

  inline constexpr const char *stage_name(pipeline_stage s) {
    static constexpr const char *names[] = {
      "input", "capture", "color_convert", "encode", "pacer", "send", "network_rtt"
    };
    return s < STAGE_COUNT ? names[s] : "?";
  }

  // ponytail: 6 lines, single-pass average.
  struct stage_latency_t {
    std::uint32_t min_us = std::numeric_limits<std::uint32_t>::max();
    std::uint32_t max_us = 0;
    double avg_us = 0.0;
    std::uint64_t n = 0;
    void submit(std::uint32_t us) {
      if (us < min_us) min_us = us;
      if (us > max_us) max_us = us;
      avg_us = (avg_us * static_cast<double>(n) + static_cast<double>(us)) / static_cast<double>(n + 1);
      ++n;
    }
    void reset() {
      min_us = std::numeric_limits<std::uint32_t>::max();
      max_us = 0;
      avg_us = 0.0;
      n = 0;
    }
  };

  struct session_info_t {
    bool active = false;
    std::string client_name;
    std::string client_app;
    std::uint32_t fps = 0, width = 0, height = 0, bitrate_kbps = 0;
    bool hdr_active = false;            ///< 10-bit/HDR negotiated with client
    bool allm_active = false;           ///< TV in game mode (CEC ALLM)
    std::chrono::system_clock::time_point started_at;
  };

  struct snapshot_t {
    stage_latency_t latency[STAGE_COUNT];
    std::uint64_t frames = 0;
    std::uint64_t bytes_sent = 0;
    std::uint64_t packets_lost = 0;
    double fps_ewma = 0.0;
    double kbps_ewma = 0.0;
    session_info_t session;
    std::uint64_t revision = 0;
    std::chrono::system_clock::time_point captured_at;
  };

  namespace telemetry {
    bool init();
    void shutdown();

    void record_latency(pipeline_stage s, std::uint32_t latency_us);
    void record_frame();
    void record_send(std::uint64_t bytes, std::uint64_t lost_delta);

    void session_begin(session_info_t info);
    void session_end();

    snapshot_t snapshot();

    // ponytail: one observer, not a subscription map. Used by adaptive_bitrate.
    // Set to nullptr to clear.
    using observer_t = std::function<void(const snapshot_t &)>;
    void set_observer(observer_t cb);
  }  // namespace telemetry
}  // namespace solarflare
