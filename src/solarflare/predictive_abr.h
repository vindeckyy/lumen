/**
 * @file src/solarflare/predictive_abr.h
 * @brief SolarFlare predictive adaptive bitrate controller.
 *
 * This is the second-generation controller. The original
 * `adaptive_bitrate` module reacts to packet loss and RTT p95 by
 * dropping the bitrate AFTER things have already gone bad. By the
 * time loss shows up on a wireless link, the user has seen 2–4
 * dropped frames.
 *
 * The predictive controller watches **jitter trend** (rate of
 * change of RTT over the last few seconds) and drops bitrate when
 * jitter is *accelerating upward* — even if RTT p50 is still
 * nominal and loss is zero. On real Wi-Fi the spike in jitter
 * 200-400 ms before loss is the single most reliable leading
 * indicator.
 *
 * Once jitter peaks and RTT settles back, the controller raises
 * bitrate in a single step (no thrash) with a longer cooldown.
 *
 * ponytail: replaces `adaptive_bitrate`'s policy function via
 * setting itself as the telemetry observer on init. Old controller
 * is left intact for the simple-use case; this controller becomes
 * the default on when `predictive_abr_enabled` is true.
 */
#pragma once

// standard includes
#include <atomic>
#include <chrono>
#include <cstdint>
#include <string>

namespace solarflare {

  struct predictive_cfg_t {
    bool enabled = true;
    std::uint32_t floor_kbps = 1000;
    std::uint32_t ceiling_kbps = 150000;
    std::uint32_t step_kbps_down = 4000;   ///< bigger step than up -- err on safety
    std::uint32_t step_kbps_up = 1500;
    std::chrono::milliseconds cooldown_down {1000};  ///< fast when things are bad
    std::chrono::milliseconds cooldown_up {8000};    ///< slow when things are good
    double jitter_accel_threshold_ms_per_s = 8.0;   ///< RTT rising 8 ms/sec = trouble
    double loss_threshold_pct = 1.0;                ///< still react to loss directly
    double rtt_threshold_ms = 80.0;                  ///< hard ceiling
  };

  struct predictive_snap_t {
    bool running = false;
    std::uint32_t current_kbps = 0;
    std::uint64_t drops_for_jitter = 0;
    std::uint64_t drops_for_loss = 0;
    std::uint64_t raises = 0;
    double last_jitter_accel = 0;          ///< ms/sec
    double last_loss_pct = 0;
    std::chrono::system_clock::time_point last_action_at;
    std::string last_reason;
  };

  namespace predictive_abr {
    bool init(const predictive_cfg_t &cfg);
    void shutdown();
    void apply_config(const predictive_cfg_t &cfg);
    predictive_snap_t snapshot();
  }  // namespace predictive_abr
}  // namespace solarflare
