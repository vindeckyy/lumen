/**
 * @file src/solarflare/session_recorder.cpp
 * @brief Definitions for the SolarFlare session recorder.
 *
 * ponytail: one JSON file per session, a single index.json that
 * holds the most-recent N records. No DB; the index is rebuilt
 * from disk on `load()` failures.
 */

// standard includes
#include <atomic>
#include <fstream>
#include <mutex>
#include <random>
#include <sstream>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "logging.h"
#include "platform/common.h"
#include "solarflare/session_recorder.h"

namespace solarflare {

  namespace session_recorder {
    namespace {
      std::atomic<bool> g_running {false};
      session_cfg_t g_cfg {};
      std::mutex g_mtx;
      session_record_t g_active {};
      bool g_active_flag = false;

      // ponytail: timestamp + 24 bits random is unique enough for one process.
      std::string gen_id() {
        auto secs = std::chrono::duration_cast<std::chrono::seconds>(
          std::chrono::system_clock::now().time_since_epoch()).count();
        std::random_device rd;
        char buf[64];
        std::snprintf(buf, sizeof(buf), "sf-%lld-%06x", static_cast<long long>(secs), rd() & 0xFFFFFFu);
        return buf;
      }

      nlohmann::json to_json(const session_record_t &r) {
        nlohmann::json j;
        j["id"] = r.id;
        j["started_at"] = std::chrono::duration_cast<std::chrono::seconds>(r.started_at.time_since_epoch()).count();
        j["ended_at"] = std::chrono::duration_cast<std::chrono::seconds>(r.ended_at.time_since_epoch()).count();
        j["duration_s"] = r.duration_seconds();
        j["client_name"] = r.client_name;
        j["client_app"] = r.client_app;
        j["fps"] = r.fps; j["width"] = r.width; j["height"] = r.height; j["bitrate_kbps"] = r.bitrate_kbps;
        j["frames"] = r.frames; j["bytes_sent"] = r.bytes_sent; j["packets_lost"] = r.packets_lost;
        j["avg_fps"] = r.avg_fps; j["avg_kbps"] = r.avg_kbps;
        // ponytail: per-stage min/max/avg arrays inline.
        nlohmann::json lat = nlohmann::json::array();
        for (std::size_t i = 0; i < STAGE_COUNT; ++i) {
          lat.push_back({ r.latency[i].min_us, r.latency[i].max_us, r.latency[i].avg_us, r.latency[i].n });
        }
        j["latency"] = lat;
        return j;
      }

      session_record_t from_json(const nlohmann::json &j) {
        session_record_t r;
        r.id = j.value("id", "");
        r.started_at = std::chrono::system_clock::time_point(std::chrono::seconds(j.value("started_at", 0LL)));
        r.ended_at = std::chrono::system_clock::time_point(std::chrono::seconds(j.value("ended_at", 0LL)));
        r.client_name = j.value("client_name", "");
        r.client_app = j.value("client_app", "");
        r.fps = j.value("fps", 0u); r.width = j.value("width", 0u); r.height = j.value("height", 0u);
        r.bitrate_kbps = j.value("bitrate_kbps", 0u);
        r.frames = j.value("frames", 0ull); r.bytes_sent = j.value("bytes_sent", 0ull); r.packets_lost = j.value("packets_lost", 0ull);
        r.avg_fps = j.value("avg_fps", 0.0); r.avg_kbps = j.value("avg_kbps", 0.0);
        if (j.contains("latency") && j["latency"].is_array()) {
          for (std::size_t i = 0; i < STAGE_COUNT && i < j["latency"].size(); ++i) {
            auto &a = j["latency"][i];
            if (a.is_array() && a.size() >= 4) {
              r.latency[i].min_us = a[0].get<std::uint32_t>();
              r.latency[i].max_us = a[1].get<std::uint32_t>();
              r.latency[i].avg_us = a[2].get<double>();
              r.latency[i].n = a[3].get<std::uint64_t>();
            }
          }
        }
        return r;
      }

      // ponytail: write-then-rename so we never leave a half-written file.
      void write_atomic(const std::filesystem::path &p, const std::string &body) {
        auto tmp = p; tmp += ".tmp";
        std::ofstream(tmp, std::ios::binary | std::ios::trunc) << body;
        std::error_code ec;
        std::filesystem::rename(tmp, p, ec);
        if (ec) BOOST_LOG(warning) << "SolarFlare session: rename failed: " << ec.message();
      }

      std::string read_file(const std::filesystem::path &p) {
        std::ifstream f(p, std::ios::binary);
        if (!f) return {};
        std::ostringstream ss; ss << f.rdbuf();
        return ss.str();
      }
    }  // namespace

    bool init(const session_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      if (g_cfg.base_dir.empty()) g_cfg.base_dir = platf::appdata() / "solarflare" / "sessions";
      std::error_code ec;
      std::filesystem::create_directories(g_cfg.base_dir, ec);
      if (ec) { BOOST_LOG(warning) << "SolarFlare session: mkdir failed: " << ec.message(); g_running = false; return false; }
      BOOST_LOG(info) << "SolarFlare session: up dir=" << g_cfg.base_dir;
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      std::lock_guard<std::mutex> lock(g_mtx);
      g_active_flag = false;
      g_active = {};
    }

    std::string begin_session(session_info_t initial) {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_active = {};
      g_active.id = gen_id();
      g_active.started_at = std::chrono::system_clock::now();
      g_active.client_name = std::move(initial.client_name);
      g_active.client_app = std::move(initial.client_app);
      g_active.fps = initial.fps; g_active.width = initial.width; g_active.height = initial.height;
      g_active.bitrate_kbps = initial.bitrate_kbps;
      g_active_flag = true;
      return g_active.id;
    }

    void end_session(const snapshot_t &s) {
      session_record_t rec;
      { std::lock_guard<std::mutex> lock(g_mtx); if (!g_active_flag) return; rec = std::move(g_active); g_active_flag = false; }
      rec.ended_at = std::chrono::system_clock::now();
      rec.frames = s.frames; rec.bytes_sent = s.bytes_sent; rec.packets_lost = s.packets_lost;
      rec.avg_fps = s.fps_ewma; rec.avg_kbps = s.kbps_ewma;
      for (std::size_t i = 0; i < STAGE_COUNT; ++i) rec.latency[i] = s.latency[i];
      auto p = g_cfg.base_dir / (rec.id + ".json");
      try { write_atomic(p, to_json(rec).dump(2)); }
      catch (const std::exception &e) { BOOST_LOG(warning) << "SolarFlare session: write failed: " << e.what(); return; }

      // ponytail: read index, prepend, cap, write back.
      auto idx = g_cfg.base_dir / "index.json";
      std::vector<session_record_t> entries;
      try {
        auto raw = read_file(idx);
        if (!raw.empty()) {
          auto j = nlohmann::json::parse(raw, nullptr, false);
          if (!j.is_discarded() && j.is_array())
            for (auto &it : j) entries.push_back(from_json(it));
        }
      } catch (...) {}
      entries.insert(entries.begin(), rec);
      if (entries.size() > g_cfg.max_index_entries) entries.resize(g_cfg.max_index_entries);
      nlohmann::json arr = nlohmann::json::array();
      for (auto &e : entries) arr.push_back(to_json(e));
      try { write_atomic(idx, arr.dump(2)); } catch (...) {}
    }

    void cancel_session() {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_active_flag = false; g_active = {};
    }

    std::vector<session_record_t> list(std::size_t max_records) {
      std::vector<session_record_t> out;
      auto idx = g_cfg.base_dir / "index.json";
      auto raw = read_file(idx);
      if (raw.empty()) return out;
      try {
        auto j = nlohmann::json::parse(raw, nullptr, false);
        if (j.is_discarded() || !j.is_array()) return out;
        std::size_t n = 0;
        for (auto &it : j) { if (n++ >= max_records) break; out.push_back(from_json(it)); }
      } catch (...) {}
      return out;
    }

    std::optional<session_record_t> load(const std::string &id) {
      if (id.empty()) return std::nullopt;
      auto raw = read_file(g_cfg.base_dir / (id + ".json"));
      if (raw.empty()) return std::nullopt;
      try {
        auto j = nlohmann::json::parse(raw, nullptr, false);
        if (j.is_discarded() || !j.is_object()) return std::nullopt;
        return from_json(j);
      } catch (...) { return std::nullopt; }
    }

    bool remove(const std::string &id) {
      if (id.empty()) return false;
      std::error_code ec;
      bool removed = std::filesystem::remove(g_cfg.base_dir / (id + ".json"), ec);
      // Rebuild index without the removed entry.
      auto idx = g_cfg.base_dir / "index.json";
      std::vector<session_record_t> entries;
      try {
        auto raw = read_file(idx);
        if (!raw.empty()) {
          auto j = nlohmann::json::parse(raw, nullptr, false);
          if (!j.is_discarded() && j.is_array())
            for (auto &it : j) {
              auto r = from_json(it);
              if (r.id != id) entries.push_back(r);
            }
        }
      } catch (...) {}
      nlohmann::json arr = nlohmann::json::array();
      for (auto &e : entries) arr.push_back(to_json(e));
      try { write_atomic(idx, arr.dump(2)); } catch (...) {}
      return removed;
    }

    bool is_recording() {
      std::lock_guard<std::mutex> lock(g_mtx);
      return g_active_flag;
    }
  }  // namespace session_recorder
}  // namespace solarflare
