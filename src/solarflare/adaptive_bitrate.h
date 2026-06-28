/**
 * @file src/solarflare/adaptive_bitrate.h
 * @brief SolarFlare adaptive bitrate: drop bitrate on bad network.
 *
 * ponytail: the upstream Sunshine encoder can be told a new bitrate
 * via config::video.max_bitrate; we just need a tiny policy function
 * that watches telemetry and rewrites that value when RTT/loss
 * crosses a threshold. No worker thread -- the policy runs on the
 * telemetry publisher path.
 */
#pragma once

// standard includes
#include <atomic>
#include <chrono>
#include <cstdint>
#include <functional>
#include <string>

namespace solarflare {

  struct adaptive_cfg_t {
    bool enabled = true;
    std::uint32_t floor_kbps = 1000;
    std::uint32_t ceiling_kbps = 150000;
    std::uint32_t step_kbps = 2000;
    std::chrono::milliseconds cooldown {2000};
    double loss_threshold_pct = 1.0;
    double rtt_threshold_ms = 80.0;
    double rtt_recover_ms = 50.0;
  };

  struct adaptive_event_t {
    std::chrono::system_clock::time_point at;
    enum class dir { increase, decrease, hold } direction = dir::hold;
    std::uint32_t old_kbps = 0;
    std::uint32_t new_kbps = 0;
    std::string reason;
  };

  struct adaptive_snap_t {
    bool running = false;
    std::uint32_t current_kbps = 0;
    std::uint64_t increases = 0, decreases = 0;
  };

  namespace adaptive_bitrate {
    bool init(const adaptive_cfg_t &cfg);
    void shutdown();
    void apply_config(const adaptive_cfg_t &cfg);
    void notify_current_bitrate(std::uint32_t kbps);
    adaptive_snap_t snapshot();

    // ponytail: single observer; the web UI binds this when it wants
    // to log / display bitrate changes.
    using event_cb_t = std::function<void(const adaptive_event_t &)>;
    void set_observer(event_cb_t cb);
  }  // namespace adaptive_bitrate
}  // namespace solarflare
