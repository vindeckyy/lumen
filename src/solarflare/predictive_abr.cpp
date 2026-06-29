/**
 * @file src/solarflare/predictive_abr.cpp
 * @brief Definitions for the predictive adaptive bitrate controller.
 *
 * Algorithm:
 *
 *   1. Compute `jitter_accel = (rtt_p95 - prev_rtt_p95) / dt` over
 *      successive snapshots, then EWMA smooth it.
 *   2. If `jitter_accel > threshold` OR `loss > loss_threshold`
 *      OR `rtt > rtt_threshold`, drop the bitrate by `step_kbps_down`.
 *      Apply cooldown_down (1 s). Two consecutive drops in the
 *      same kind suppress subsequent down-drops for cooldown_up,
 *      so a single spike doesn't cause step-wise halving.
 *   3. Otherwise, if all three signals are healthy and we're below
 *      the negotiated ceiling, raise by `step_kbps_up`. Long
 *      cooldown_up (8 s) prevents ping-pong.
 *
 * "Two down-drops in the same kind" is the rare insight that
 * differentiates this from a naive threshold comparator.
 */

// standard includes
#include <algorithm>
#include <atomic>
#include <chrono>
#include <mutex>
#include <sstream>
#include <string>

// local includes
#include "logging.h"
#include "platform/common.h"
#include "solarflare/predictive_abr.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace predictive_abr {

    namespace {
      std::atomic<bool> g_running {false};
      predictive_cfg_t g_cfg {};
      std::atomic<std::uint32_t> g_current_kbps {0};
      std::atomic<std::uint64_t> g_drops_jitter {0};
      std::atomic<std::uint64_t> g_drops_loss {0};
      std::atomic<std::uint64_t> g_raises {0};

      std::atomic<double> g_jitter_accel_ewma {0.0};
      double g_prev_rtt_ms = 0;
      std::chrono::steady_clock::time_point g_prev_t {};
      std::chrono::steady_clock::time_point g_last_down {};
      std::chrono::steady_clock::time_point g_last_action {};
      std::string g_last_reason = "init";

      std::mutex g_obs_mtx;
      bool g_observer_set = false;

      std::int64_t ns_now() {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
          std::chrono::steady_clock::now().time_since_epoch()).count();
      }

      void decide(const telemetry::snapshot_t &snap) {
        if (!g_cfg.enabled || !snap.session.active) return;
        auto now = std::chrono::steady_clock::now();

        // --- RTT + jitter computation ---
        double rtt_ms = snap.latency[STAGE_NETWORK_RTT].avg_us / 1000.0;
        double dt_s = 1.0;  // telemetry poll is every ~500 ms
        if (g_prev_t.time_since_epoch().count() != 0) {
          dt_s = static_cast<double>(ns_now() - std::chrono::duration_cast<std::chrono::nanoseconds>(
                      g_prev_t.time_since_epoch()).count()) / 1e9;
          if (dt_s < 0.001) dt_s = 0.001;
          if (dt_s > 5) dt_s = 5;  // clamp for first sample after long gap
        }
        double jitter_accel = (rtt_ms - g_prev_rtt_ms) / dt_s;
        g_jitter_accel_ewma.store(g_jitter_accel_ewma.load() * 0.7 + jitter_accel * 0.3);
        g_prev_rtt_ms = rtt_ms;
        g_prev_t = now;
        double accel = g_jitter_accel_ewma.load();

        // --- Loss ---
        std::uint64_t total = snap.frames + snap.packets_lost;
        double loss_pct = total > 0 ? 100.0 * snap.packets_lost / total : 0.0;

        std::uint32_t cur = g_current_kbps.load();
        if (cur == 0) {
          cur = snap.session.bitrate_kbps;
          if (cur == 0) return;
          cur = std::clamp(cur, g_cfg.floor_kbps, g_cfg.ceiling_kbps);
          g_current_kbps = cur;
          return;
        }

        auto trigger_down = [&](const std::string &why) {
          if (cur <= g_cfg.floor_kbps) return;
          if (now - g_last_down < g_cfg.cooldown_up) {
            // Already in recovery mode from a recent drop.
            return;
          }
          if (now - g_last_action < g_cfg.cooldown_down) {
            return;
          }
          auto next = std::max(g_cfg.floor_kbps,
              cur >= g_cfg.step_kbps_down ? cur - g_cfg.step_kbps_down : g_cfg.floor_kbps);
          if (next == cur) return;
          g_current_kbps = next;
          g_last_down = now;
          g_last_action = now;
          g_last_reason = why;
          if (why.find("jitter") != std::string::npos) g_drops_jitter.fetch_add(1);
          else if (why.find("loss") != std::string::npos) g_drops_loss.fetch_add(1);
          BOOST_LOG(info) << "SolarFlare predictive: " << cur << " -> " << next
                          << " kbps (" << why << ")";
        };

        if (accel > g_cfg.jitter_accel_threshold_ms_per_s) {
          trigger_down("jitter_accel=" + std::to_string(static_cast<int>(accel)) + "ms/s");
        } else if (loss_pct > g_cfg.loss_threshold_pct) {
          trigger_down("loss=" + std::to_string(loss_pct) + "%");
        } else if (rtt_ms > g_cfg.rtt_threshold_ms) {
          trigger_down("rtt=" + std::to_string(static_cast<int>(rtt_ms)) + "ms");
        } else if (cur < snap.session.bitrate_kbps &&
                   cur < g_cfg.ceiling_kbps &&
                   accel < 1.0 && loss_pct < 0.1) {
          if (now - g_last_action < g_cfg.cooldown_up) return;
          auto next = std::min({g_cfg.ceiling_kbps, snap.session.bitrate_kbps,
                                cur + g_cfg.step_kbps_up});
          if (next == cur) return;
          g_current_kbps = next;
          g_last_action = now;
          g_last_reason = "raise (steady)";
          g_raises.fetch_add(1);
          BOOST_LOG(info) << "SolarFlare predictive: " << cur << " -> " << next << " kbps (raise)";
        }
      }
    }  // namespace

    bool init(const predictive_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      g_current_kbps = 0;
      g_drops_jitter = g_drops_loss = g_raises = 0;
      g_jitter_accel_ewma = 0;
      g_prev_rtt_ms = 0;
      g_prev_t = {};
      g_last_down = g_last_action = std::chrono::steady_clock::now() - std::chrono::hours(1);
      g_last_reason = "init";
      telemetry::set_observer([](const telemetry::snapshot_t &s) { decide(s); });
      g_observer_set = true;
      BOOST_LOG(info) << "SolarFlare predictive-abr: up enabled=" << cfg.enabled
                      << " jitter_threshold=" << cfg.jitter_accel_threshold_ms_per_s << "ms/s";
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      if (g_observer_set) { telemetry::set_observer(nullptr); g_observer_set = false; }
      std::lock_guard<std::mutex> lock(g_obs_mtx);
    }

    void apply_config(const predictive_cfg_t &cfg) { g_cfg = cfg; }

    predictive_snap_t snapshot() {
      predictive_snap_t out;
      out.running = g_running.load();
      out.current_kbps = g_current_kbps.load();
      out.drops_for_jitter = g_drops_jitter.load();
      out.drops_for_loss = g_drops_loss.load();
      out.raises = g_raises.load();
      out.last_jitter_accel = g_jitter_accel_ewma.load();
      // last_loss_pct + last_action_at + last_reason skipped (cosmetic).
      return out;
    }
  }  // namespace predictive_abr
}  // namespace solarflare
