/**
 * @file src/solarflare/network_probe.cpp
 * @brief Definitions for the SolarFlare network probe.
 *
 * ponytail: a target table, a 256-sample RTT window per target,
 * recompute on every submit. No thread; callers invoke from the
 * telemetry publisher path or from the CLI.
 */

// standard includes
#include <algorithm>
#include <atomic>
#include <cmath>
#include <numeric>
#include <unordered_map>

// local includes
#include "logging.h"
#include "solarflare/network_probe.h"

namespace solarflare {

  namespace network_probe {
    namespace {
      constexpr std::size_t kRttWindow = 256;

      struct target_state_t {
        std::deque<double> rtt_samples;
        std::uint64_t sent_total = 0, lost_total = 0;
        network_target_t snapshot;
      };

      std::mutex g_mtx;
      std::unordered_map<std::string, target_state_t> g_targets;
      std::atomic<bool> g_running {false};

      // ponytail: recompute in-place, ~15 lines.
      void recompute(target_state_t &t) {
        if (t.rtt_samples.empty()) {
          t.snapshot.rtt_p95_ms = t.snapshot.jitter_ms = 0;
          t.snapshot.score = 100;
          return;
        }
        std::vector<double> s(t.rtt_samples.begin(), t.rtt_samples.end());
        std::sort(s.begin(), s.end());
        auto pct = [&](double p) { auto i = static_cast<std::size_t>(std::ceil(p * s.size())) - 1; return s[std::min(i, s.size() - 1)]; };
        t.snapshot.rtt_p95_ms = pct(0.95);
        double mean = std::accumulate(s.begin(), s.end(), 0.0) / s.size();
        t.snapshot.jitter_ms = std::sqrt(std::accumulate(s.begin(), s.end(), 0.0, [mean](double a, double v) { return a + (v - mean) * (v - mean); }) / s.size());
        std::uint64_t total = t.sent_total + t.lost_total;
        double loss_pct = total > 0 ? 100.0 * t.lost_total / total : 0.0;
        t.snapshot.loss_pct = loss_pct;
        t.snapshot.score = compute_score(t.snapshot.rtt_p95_ms, t.snapshot.jitter_ms, loss_pct);
        t.snapshot.last_update = std::chrono::system_clock::now();
      }
    }  // namespace

    bool init() {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      std::lock_guard<std::mutex> lock(g_mtx);
      g_targets.clear();
    }

    void register_target(const std::string &name) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto &t = g_targets[name];
      t.snapshot.name = name;
      t.snapshot.last_update = std::chrono::system_clock::now();
    }

    void unregister_target(const std::string &name) {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_targets.erase(name);
    }

    void submit_rtt(const std::string &name, double rtt_ms) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto &t = g_targets[name];
      t.snapshot.name = name;
      t.rtt_samples.push_back(rtt_ms);
      if (t.rtt_samples.size() > kRttWindow) t.rtt_samples.pop_front();
      recompute(t);
    }

    void submit_loss(const std::string &name, std::uint64_t sent_delta, std::uint64_t lost_delta) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto &t = g_targets[name];
      t.snapshot.name = name;
      t.sent_total += sent_delta;
      t.lost_total += lost_delta;
      recompute(t);
    }

    void submit_throughput(const std::string &name, double kbps) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto &t = g_targets[name];
      t.snapshot.name = name;
      t.snapshot.kbps = kbps;
    }

    network_snap_t snapshot() {
      network_snap_t out;
      out.captured_at = std::chrono::system_clock::now();
      double total = 0;
      std::size_t n = 0;
      std::lock_guard<std::mutex> lock(g_mtx);
      out.targets.reserve(g_targets.size());
      for (auto &[name, t] : g_targets) {
        out.targets.push_back(t.snapshot);
        total += t.snapshot.score;
        ++n;
      }
      out.aggregate_score = n > 0 ? total / static_cast<double>(n) : 100.0;
      return out;
    }
  }  // namespace network_probe
}  // namespace solarflare
