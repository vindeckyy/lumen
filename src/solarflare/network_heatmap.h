/**
 * @file src/solarflare/network_heatmap.h
 * @brief SolarFlare network condition heatmap: 60-second RTT/loss
 *        bucket history per target.
 *
 * ponytail: a heatmap is just a 60-element ring of buckets, each
 * holding the worst RTT and the loss count in its 1-second
 * window. The web UI renders the ring as a row of 60 colour-coded
 * cells; you can literally see the wireless interference burst.
 *
 * We don't poll: the existing telemetry path (which already
 * receives an RTT sample every second or two from the streaming
 * loop) feeds into the bucket at submit_rtt() / submit_loss().
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>
#include <mutex>
#include <string>
#include <vector>

namespace solarflare {

  /** @brief One 1-second bucket in the heatmap. */
  struct heatmap_bucket_t {
    std::chrono::system_clock::time_point second_start;
    double max_rtt_ms = 0;
    std::uint64_t lost = 0;
    std::uint64_t sent = 0;
  };

  /** @brief A target's 60-second heatmap + aggregate score. */
  struct heatmap_target_t {
    std::string name;
    std::vector<heatmap_bucket_t> buckets;  // newest at the front
    double score = 100;                      // 0-100
  };

  struct heatmap_snap_t {
    std::vector<heatmap_target_t> targets;
    std::chrono::system_clock::time_point captured_at;
  };

  namespace network_heatmap {
    void init();
    void shutdown();
    void register_target(const std::string &name);
    void unregister_target(const std::string &name);
    void submit_rtt(const std::string &name, double rtt_ms);
    void submit_loss(const std::string &name, std::uint64_t sent_delta, std::uint64_t lost_delta);
    heatmap_snap_t snapshot();
  }  // namespace network_heatmap
}  // namespace solarflare
