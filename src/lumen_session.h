/**
 * @file src/lumen_session.h
 * @brief Per-session streaming metrics for the Lumen live dashboard.
 *
 * The C++ encode loop calls `record_sample()` once per encoded frame;
 * `list_active()` + `get(id)` render the JSON the web dashboard polls.
 *
 * std::map under a single mutex. Cardinality is small
 * (one entry per active stream, typically 0–4). If a single host ever
 * fans out to >50 concurrent streams, swap to a lock-free or sharded
 * map — but measure first.
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <unordered_map>

namespace lumen {
  namespace session {

    /// Per-stream snapshot — what the dashboard renders.
    struct Metrics {
      std::string id;             ///< UUID-ish, generated on start.
      std::string app_name;       ///< From the app entry that was launched.
      std::string client_name;    ///< Moonlight client name (or "<unknown>").
      std::chrono::system_clock::time_point started_at;
      int64_t     started_at_ms = 0;

      // Encoder + transport
      double      encoder_fps   = 0.0;
      double      target_fps    = 0.0;
      int64_t     bitrate_kbps  = 0;
      int64_t     rtt_ms        = 0;
      double      encode_ms     = 0.0;
      int64_t     frames_sent   = 0;
      int64_t     frames_dropped = 0;
    };

    /**
     * @brief Start tracking a new session. Returns the session id.
     * @param app_name The configured app's display name.
     * @param client_name The Moonlight client's display name.
     */
    std::string start(const std::string &app_name, const std::string &client_name);

    /**
     * @brief Update the metrics for an existing session.
     * @return true if the session is known; false if it has been stopped
     *         (e.g. client disconnected) between start() and now.
     */
    bool record_sample(const std::string &id, const Metrics &update);

    /**
     * @brief Stop tracking a session. Idempotent (no-op if already gone).
     */
    void stop(const std::string &id);

    /**
     * @brief Snapshot every active session as a JSON array string.
     */
    std::string list_active_json();

    /**
     * @brief Snapshot a single session as JSON, or empty string if not found.
     */
    std::string get_json(const std::string &id);

  }  // namespace session
}  // namespace lumen