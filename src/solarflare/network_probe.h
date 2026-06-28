/**
 * @file src/solarflare/network_probe.h
 * @brief SolarFlare network quality: a single score function.
 *
 * ponytail: a passive quality estimator doesn't need a module --
 * it's one function and a tiny target table. The score formula is
 * documented inline so a future reviewer can sanity-check it.
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>
#include <deque>
#include <mutex>
#include <string>
#include <vector>

namespace solarflare {

  struct network_target_t {
    std::string name;
    double rtt_p95_ms = 0, jitter_ms = 0, loss_pct = 0, kbps = 0, score = 100;
    std::chrono::system_clock::time_point last_update;
  };

  struct network_snap_t {
    std::vector<network_target_t> targets;
    double aggregate_score = 100;
    std::chrono::system_clock::time_point captured_at;
  };

  namespace network_probe {
    bool init();
    void shutdown();

    void register_target(const std::string &name);
    void unregister_target(const std::string &name);
    void submit_rtt(const std::string &name, double rtt_ms);
    void submit_loss(const std::string &name, std::uint64_t sent_delta, std::uint64_t lost_delta);
    void submit_throughput(const std::string &name, double kbps);

    network_snap_t snapshot();

    // ponytail: the whole policy in one expression. clamp_score is the only edge case.
    inline double clamp_score(double s) { return s < 0 ? 0 : (s > 100 ? 100 : s); }
    inline double compute_score(double rtt_p95_ms, double jitter_ms, double loss_pct) {
      return clamp_score(100.0
        - 2.0 * std::max(0.0, rtt_p95_ms - 30.0)
        - 5.0 * std::max(0.0, loss_pct)
        - 1.0 * std::max(0.0, jitter_ms - 5.0));
    }
  }  // namespace network_probe
}  // namespace solarflare
