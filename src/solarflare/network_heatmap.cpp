/**
 * @file src/solarflare/network_heatmap.cpp
 * @brief Definitions for the network condition heatmap.
 */

// standard includes
#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <mutex>
#include <unordered_map>

// local includes
#include "logging.h"
#include "solarflare/network_heatmap.h"

namespace solarflare {

  namespace network_heatmap {

    namespace {
      constexpr std::size_t kBuckets = 60;  // 60 seconds

      struct target_state_t {
        std::vector<heatmap_bucket_t> buckets;  // indexed 0..kBuckets, oldest first
        std::mutex mtx;
        std::uint64_t pending_sent = 0;
        std::uint64_t pending_lost = 0;
        // Each call's pending rtt gets folded into the current bucket.
        double pending_max_rtt = 0;
      };

      std::mutex g_mtx;
      std::unordered_map<std::string, target_state_t> g_targets;
      std::atomic<bool> g_running {false};
      std::chrono::system_clock::time_point g_epoch;

      std::chrono::system_clock::time_point now() {
        return std::chrono::system_clock::now();
      }

      // Index of the current bucket for the wall-clock second.
      std::size_t bucket_index(std::chrono::system_clock::time_point t) {
        auto secs = std::chrono::duration_cast<std::chrono::seconds>(
          t.time_since_epoch()).count();
        auto epoch_secs = std::chrono::duration_cast<std::chrono::seconds>(
          g_epoch.time_since_epoch()).count();
        return (kBuckets - 1) - (secs - epoch_secs) % kBuckets;  // newest at high index
      }

      void ensure_target_unlocked(target_state_t &t) {
        if (t.buckets.size() != kBuckets) {
          t.buckets.assign(kBuckets, heatmap_bucket_t {});
        }
        // Fill zero-counts with previous state where reasonable.
      }

      // Age out buckets: anything older than 60s becomes zeroed.
      void roll_buckets() {
        auto now_t = now();
        auto cut = now_t - std::chrono::seconds(kBuckets);
        for (auto &[name, t] : g_targets) {
          std::lock_guard<std::mutex> lock(t.mtx);
          for (auto &b : t.buckets) {
            if (b.second_start < cut && b.max_rtt_ms == 0) {
              b.second_start = {};
              b.lost = b.sent = 0;
            }
          }
        }
      }

      // ponytail: each target gets a single composite score from the
      // 60 buckets. Formula: penalise max RTT > 30 ms linearly +
      // loss in any bucket.
      double compute_score(const target_state_t &t) {
        double max_rtt = 0;
        std::uint64_t lost = 0;
        std::uint64_t sent = 0;
        for (auto &b : t.buckets) {
          if (b.max_rtt_ms > max_rtt) max_rtt = b.max_rtt_ms;
          lost += b.lost;
          sent += b.sent;
        }
        double loss_pct = sent > 0 ? 100.0 * lost / sent : 0.0;
        double s = 100.0
                 - 2.0 * std::max(0.0, max_rtt - 30.0)
                 - 5.0 * std::max(0.0, loss_pct);
        return std::max(0.0, std::min(100.0, s));
      }
    }  // namespace

    void init() {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return;
      g_epoch = now();
      BOOST_LOG(info) << "SolarFlare heatmap: up";
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      std::lock_guard<std::mutex> lock(g_mtx);
      g_targets.clear();
    }

    void register_target(const std::string &name) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto &t = g_targets[name];
      t.buckets.assign(kBuckets, heatmap_bucket_t {});
    }

    void unregister_target(const std::string &name) {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_targets.erase(name);
    }

    void submit_rtt(const std::string &name, double rtt_ms) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto it = g_targets.find(name);
      if (it == g_targets.end()) { register_target(name); it = g_targets.find(name); }
      auto &t = it->second;
      std::lock_guard<std::mutex> inner(t.mtx);
      auto idx = bucket_index(now());
      if (idx >= t.buckets.size()) t.buckets.resize(kBuckets);
      if (t.buckets[idx].second_start.time_since_epoch().count() == 0) {
        t.buckets[idx].second_start = now();
      }
      if (rtt_ms > t.buckets[idx].max_rtt_ms) t.buckets[idx].max_rtt_ms = rtt_ms;
      if (rtt_ms > t.pending_max_rtt) t.pending_max_rtt = rtt_ms;
    }

    void submit_loss(const std::string &name, std::uint64_t sent_delta, std::uint64_t lost_delta) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto it = g_targets.find(name);
      if (it == g_targets.end()) return;
      auto &t = it->second;
      std::lock_guard<std::mutex> inner(t.mtx);
      auto idx = bucket_index(now());
      if (idx >= t.buckets.size()) t.buckets.resize(kBuckets);
      t.buckets[idx].sent += sent_delta;
      t.buckets[idx].lost += lost_delta;
    }

    heatmap_snap_t snapshot() {
      heatmap_snap_t out;
      out.captured_at = now();
      roll_buckets();
      std::lock_guard<std::mutex> lock(g_mtx);
      for (auto &[name, t] : g_targets) {
        std::lock_guard<std::mutex> inner(t.mtx);
        heatmap_target_t ht;
        ht.name = name;
        // Newest first for the API consumer.
        ht.buckets.assign(t.buckets.rbegin(), t.buckets.rend());
        ht.score = compute_score(t);
        out.targets.push_back(ht);
      }
      return out;
    }
  }  // namespace network_heatmap
}  // namespace solarflare
