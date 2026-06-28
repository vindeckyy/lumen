/**
 * @file src/solarflare/session_recorder.h
 * @brief SolarFlare session recorder: JSON metadata per session.
 *
 * ponytail: one JSON file per session, plus an index file. No DB,
 * no fancy concurrency. The web UI can load the index on demand.
 */
#pragma once

// standard includes
#include <atomic>
#include <chrono>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

// local includes
#include "solarflare/telemetry.h"

namespace solarflare {

  struct session_record_t {
    std::string id;
    std::chrono::system_clock::time_point started_at, ended_at;
    std::string client_name, client_app;
    std::uint32_t fps = 0, width = 0, height = 0, bitrate_kbps = 0;
    std::uint64_t frames = 0, bytes_sent = 0, packets_lost = 0;
    double avg_fps = 0, avg_kbps = 0;
    stage_latency_t latency[STAGE_COUNT];

    std::uint64_t duration_seconds() const {
      return std::chrono::duration_cast<std::chrono::seconds>(ended_at - started_at).count();
    }
  };

  struct session_cfg_t {
    bool enabled = true;
    std::filesystem::path base_dir;  // ponytail: defaults to <appdata>/solarflare/sessions
    std::uint32_t max_index_entries = 500;
  };

  namespace session_recorder {
    bool init(const session_cfg_t &cfg);
    void shutdown();

    std::string begin_session(session_info_t initial);
    void end_session(const snapshot_t &final);
    void cancel_session();

    std::vector<session_record_t> list(std::size_t max_records);
    std::optional<session_record_t> load(const std::string &id);
    bool remove(const std::string &id);
    bool is_recording();
  }  // namespace session_recorder
}  // namespace solarflare
