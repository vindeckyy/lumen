/**
 * @file src/lumen_metrics.h
 * @brief Tiny Prometheus-style metrics registry for Lumen.
 *
 * std::atomic per counter, no framework. The streaming
 * engine pokes these in place; the web server renders them on
 * `GET /api/v1/metrics`. No labels for now — add when a single
 * counter genuinely needs per-stream / per-encoder breakdown.
 */
#pragma once

// standard includes
#include <atomic>
#include <cstdint>
#include <string>

namespace lumen {

  /// Gauge: 1 while the host is accepting streams, 0 during shutdown.
  extern std::atomic<int64_t> metric_up;

  /// Gauge: number of currently active streams.
  extern std::atomic<int64_t> metric_active_streams;

  /// Counter: total bytes sent over the wire to clients.
  extern std::atomic<int64_t> metric_bytes_sent;

  /// Counter: total frames encoded since process start.
  extern std::atomic<int64_t> metric_frames_encoded;

  /// Counter: total frames dropped (encoder back-pressure or client slow).
  extern std::atomic<int64_t> metric_frames_dropped;

  /// Counter: total HTTP requests handled by the confighttp server.
  extern std::atomic<int64_t> metric_http_requests;

  /**
   * @brief Render all metrics in Prometheus text exposition format.
   * @return The full text/plain body for `GET /api/v1/metrics`.
   *
   * If cardinality ever matters, switch to a proper registry
   * (prometheus-cpp); for a handful of host-level counters, this
   * implementation is the simplest correct one.
   */
  std::string render_metrics();

}  // namespace lumen