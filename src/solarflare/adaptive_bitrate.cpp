/**
 * @file src/solarflare/adaptive_bitrate.cpp
 * @brief Definitions for the SolarFlare adaptive bitrate controller.
 *
 * ponytail: one telemetry subscription, one cooldown, one policy
 * function. The policy is the only place where the "should we drop
 * bitrate?" decision lives; everything else is bookkeeping.
 */

// standard includes
#include <algorithm>
#include <atomic>
#include <chrono>
#include <map>
#include <mutex>
#include <sstream>
#include <vector>

// local includes
#include "logging.h"
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace adaptive_bitrate {
    namespace {
      std::atomic<bool> g_running {false};
      adaptive_cfg_t g_cfg {};
      std::atomic<std::uint32_t> g_current_kbps {0};
      std::atomic<std::uint64_t> g_inc {0}, g_dec {0};
      std::chrono::steady_clock::time_point g_last {};
      std::mutex g_obs_mtx;
      event_cb_t g_observer;

      // ponytail: the whole policy in ~25 lines.
      void evaluate(const telemetry::snapshot_t &snap) {
        if (!g_cfg.enabled || !snap.session.active) return;
        auto now = std::chrono::steady_clock::now();
        if (now - g_last < g_cfg.cooldown) return;
        double rtt_ms = snap.latency[STAGE_NETWORK_RTT].avg_us / 1000.0;
        std::uint64_t total = snap.frames + snap.packets_lost;  // ponytail: approximation
        double loss_pct = total > 0 ? 100.0 * snap.packets_lost / total : 0.0;
        std::uint32_t cur = g_current_kbps.load();
        if (cur == 0) {
          cur = snap.session.bitrate_kbps;
          if (cur == 0) return;
          cur = std::clamp(cur, g_cfg.floor_kbps, g_cfg.ceiling_kbps);
          g_current_kbps = cur;
          return;
        }
        if (loss_pct <= g_cfg.loss_threshold_pct && rtt_ms <= g_cfg.rtt_threshold_ms) return;
        if (cur <= g_cfg.floor_kbps) return;
        auto next = std::max(g_cfg.floor_kbps, cur - g_cfg.step_kbps);
        if (next == cur) return;
        adaptive_event_t ev { .at = std::chrono::system_clock::now(), .direction = adaptive_event_t::dir::decrease, .old_kbps = cur, .new_kbps = next, .reason = "policy" };
        g_current_kbps = next;
        g_dec.fetch_add(1);
        g_last = now;
        event_cb_t obs;
        { std::lock_guard<std::mutex> lock(g_obs_mtx); obs = g_observer; }
        if (obs) try { obs(ev); } catch (...) {}
        BOOST_LOG(info) << "SolarFlare adaptive: " << cur << " -> " << next << " kbps";
      }
    }  // namespace

    bool init(const adaptive_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      g_current_kbps = 0;
      g_inc = g_dec = 0;
      g_last = std::chrono::steady_clock::now() - std::chrono::hours(1);
      telemetry::set_observer([](const telemetry::snapshot_t &s) { evaluate(s); });
      BOOST_LOG(info) << "SolarFlare adaptive: up enabled=" << cfg.enabled;
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      telemetry::set_observer(nullptr);
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = nullptr;
    }

    void apply_config(const adaptive_cfg_t &cfg) { g_cfg = cfg; }

    void notify_current_bitrate(std::uint32_t kbps) { g_current_kbps = kbps; }

    adaptive_snap_t snapshot() {
      return { .running = g_running.load(), .current_kbps = g_current_kbps.load(), .increases = g_inc.load(), .decreases = g_dec.load() };
    }

    void set_observer(event_cb_t cb) {
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = std::move(cb);
    }
  }  // namespace adaptive_bitrate
}  // namespace solarflare
