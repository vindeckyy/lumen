/**
 * @file src/solarflare/telemetry.cpp
 * @brief Definitions for the SolarFlare telemetry engine.
 *
 * ponytail: per-stage min/max/avg, four atomics, one tiny mutex for
 * the session info, and a single observer slot for the one downstream
 * consumer that exists today (adaptive_bitrate). No publisher thread,
 * no subscriber map, no SSE/HTTP plumbing -- that's the web UI's job.
 */

// standard includes
#include <chrono>
#include <mutex>

// local includes
#include "logging.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace telemetry {
    namespace {
      stage_latency_t g_latency[STAGE_COUNT];
      std::atomic<std::uint64_t> g_frames {0};
      std::atomic<std::uint64_t> g_bytes {0};
      std::atomic<std::uint64_t> g_packets_lost {0};
      std::atomic<double> g_fps_ewma {0.0};
      std::atomic<double> g_kbps_ewma {0.0};
      std::atomic<std::int64_t> g_last_tick_ns {0};

      std::mutex g_session_mtx;
      session_info_t g_session;

      std::atomic<std::uint64_t> g_revision {0};
      std::atomic<bool> g_running {false};

      // ponytail: one observer, not a subscription map.
      std::mutex g_obs_mtx;
      observer_t g_observer;

      std::int64_t now_ns() {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
          std::chrono::steady_clock::now().time_since_epoch()).count();
      }
    }  // namespace

    bool init() {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_frames = 0;
      g_bytes = 0;
      g_packets_lost = 0;
      g_fps_ewma = 0.0;
      g_kbps_ewma = 0.0;
      g_revision = 0;
      g_last_tick_ns = now_ns();
      g_session = session_info_t {};
      for (auto &t : g_latency) t.reset();
      BOOST_LOG(info) << "SolarFlare telemetry: up";
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = nullptr;
    }

    void record_latency(pipeline_stage s, std::uint32_t latency_us) {
      if (s >= STAGE_COUNT) return;
      g_latency[s].submit(latency_us);
      g_revision.fetch_add(1, std::memory_order_acq_rel);
      observer_t obs;
      { std::lock_guard<std::mutex> lock(g_obs_mtx); obs = g_observer; }
      if (obs) { try { obs(snapshot()); } catch (...) {} }
    }

    void record_frame() {
      g_frames.fetch_add(1, std::memory_order_relaxed);
      auto now = now_ns();
      auto last = g_last_tick_ns.exchange(now, std::memory_order_relaxed);
      if (last != 0) {
        double dt = static_cast<double>(now - last) / 1e9;
        if (dt > 0.0001) {
          double fps = 1.0 / dt;
          double prev = g_fps_ewma.load();
          g_fps_ewma.store(prev == 0.0 ? fps : prev * 0.8 + fps * 0.2);
        }
      }
    }

    void record_send(std::uint64_t bytes, std::uint64_t lost_delta) {
      g_bytes.fetch_add(bytes, std::memory_order_relaxed);
      g_packets_lost.fetch_add(lost_delta, std::memory_order_relaxed);
      auto now = now_ns();
      auto last = g_last_tick_ns.load();
      if (last != 0) {
        double dt = static_cast<double>(now - last) / 1e9;
        if (dt > 0.0001) {
          double kbps = (bytes * 8.0 / 1000.0) / dt;
          double prev = g_kbps_ewma.load();
          g_kbps_ewma.store(prev == 0.0 ? kbps : prev * 0.7 + kbps * 0.3);
        }
      }
    }

    void session_begin(session_info_t info) {
      {
        std::lock_guard<std::mutex> lock(g_session_mtx);
        g_session = std::move(info);
        g_session.active = true;
        g_session.started_at = std::chrono::system_clock::now();
      }
      for (auto &t : g_latency) t.reset();
      BOOST_LOG(info) << "SolarFlare telemetry: session begin";
    }

    void session_end() {
      std::lock_guard<std::mutex> lock(g_session_mtx);
      g_session = session_info_t {};
    }

    snapshot_t snapshot() {
      snapshot_t out;
      out.revision = g_revision.load(std::memory_order_acquire);
      out.captured_at = std::chrono::system_clock::now();
      for (std::size_t i = 0; i < STAGE_COUNT; ++i) out.latency[i] = g_latency[i];
      out.frames = g_frames.load();
      out.bytes_sent = g_bytes.load();
      out.packets_lost = g_packets_lost.load();
      out.fps_ewma = g_fps_ewma.load();
      out.kbps_ewma = g_kbps_ewma.load();
      {
        std::lock_guard<std::mutex> lock(g_session_mtx);
        out.session = g_session;
      }
      return out;
    }

    void set_observer(observer_t cb) {
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = std::move(cb);
    }
  }  // namespace telemetry
}  // namespace solarflare
