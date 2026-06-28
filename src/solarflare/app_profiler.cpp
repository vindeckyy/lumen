/**
 * @file src/solarflare/app_profiler.cpp
 * @brief Definitions for the SolarFlare app profiler.
 *
 * ponytail: JSON in, JSON out. Match is a substring on the exec
 * basename; first hit by priority wins.
 */

// standard includes
#include <algorithm>
#include <atomic>
#include <fstream>
#include <mutex>
#include <optional>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "logging.h"
#include "platform/common.h"
#include "solarflare/app_profiler.h"

namespace solarflare {

  namespace app_profiler {
    namespace {
      std::atomic<bool> g_running {false};
      app_profiler_cfg_t g_cfg {};
      std::mutex g_mtx;
      std::vector<app_profile_t> g_profiles;
      std::optional<std::string> g_active;

      nlohmann::json to_json(const app_profile_t &p) {
        nlohmann::json j;
        j["name"] = p.name; j["match"] = p.match;
        j["priority"] = p.priority; j["enabled"] = p.enabled;
        if (p.fps) j["fps"] = *p.fps;
        if (p.width) j["width"] = *p.width;
        if (p.height) j["height"] = *p.height;
        if (p.bitrate_kbps) j["bitrate_kbps"] = *p.bitrate_kbps;
        if (p.encoder) j["encoder"] = *p.encoder;
        if (p.codec) j["codec"] = *p.codec;
        return j;
      }

      app_profile_t from_json(const nlohmann::json &j) {
        app_profile_t p;
        p.name = j.value("name", "");
        p.match = j.value("match", "");
        p.priority = j.value("priority", 100);
        p.enabled = j.value("enabled", true);
        if (j.contains("fps")) p.fps = j["fps"].get<std::uint32_t>();
        if (j.contains("width")) p.width = j["width"].get<std::uint32_t>();
        if (j.contains("height")) p.height = j["height"].get<std::uint32_t>();
        if (j.contains("bitrate_kbps")) p.bitrate_kbps = j["bitrate_kbps"].get<std::uint32_t>();
        if (j.contains("encoder")) p.encoder = j["encoder"].get<std::string>();
        if (j.contains("codec")) p.codec = j["codec"].get<std::string>();
        return p;
      }
    }  // namespace

    bool init(const app_profiler_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      if (g_cfg.profiles_path.empty()) g_cfg.profiles_path = platf::appdata() / "solarflare" / "profiles.json";
      reload();
      BOOST_LOG(info) << "SolarFlare app profiler: up path=" << g_cfg.profiles_path;
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      std::lock_guard<std::mutex> lock(g_mtx);
      g_profiles.clear();
      g_active.reset();
    }

    void reload() {
      std::lock_guard<std::mutex> lock(g_mtx);
      g_profiles.clear();
      std::ifstream f(g_cfg.profiles_path);
      if (!f) return;
      try {
        nlohmann::json j; f >> j;
        auto &arr = j.is_array() ? j : (j.contains("profiles") && j["profiles"].is_array() ? j["profiles"] : nlohmann::json::array());
        for (auto &it : arr) if (it.is_object()) g_profiles.push_back(from_json(it));
      } catch (const std::exception &e) {
        BOOST_LOG(warning) << "SolarFlare app profiler: parse failed: " << e.what();
      }
    }

    void save() {
      std::lock_guard<std::mutex> lock(g_mtx);
      nlohmann::json arr = nlohmann::json::array();
      for (auto &p : g_profiles) arr.push_back(to_json(p));
      std::filesystem::create_directories(g_cfg.profiles_path.parent_path());
      std::ofstream(g_cfg.profiles_path, std::ios::binary | std::ios::trunc) << arr.dump(2);
    }

    std::vector<app_profile_t> list() {
      std::lock_guard<std::mutex> lock(g_mtx);
      return g_profiles;
    }

    void upsert(const app_profile_t &p) {
      if (p.name.empty()) return;
      std::lock_guard<std::mutex> lock(g_mtx);
      auto it = std::find_if(g_profiles.begin(), g_profiles.end(), [&](const app_profile_t &x) { return x.name == p.name; });
      if (it != g_profiles.end()) *it = p; else g_profiles.push_back(p);
      std::sort(g_profiles.begin(), g_profiles.end(), [](const app_profile_t &a, const app_profile_t &b) { return a.priority < b.priority; });
    }

    bool remove(const std::string &name) {
      std::lock_guard<std::mutex> lock(g_mtx);
      auto it = std::find_if(g_profiles.begin(), g_profiles.end(), [&](const app_profile_t &p) { return p.name == name; });
      if (it == g_profiles.end()) return false;
      g_profiles.erase(it);
      return true;
    }

    std::optional<app_profile_t> find_match(const std::string &exec_path) {
      if (exec_path.empty()) return std::nullopt;
      // ponytail: extract basename with a 1-liner.
      auto pos = exec_path.find_last_of("/\\");
      std::string base = pos == std::string::npos ? exec_path : exec_path.substr(pos + 1);
      std::lock_guard<std::mutex> lock(g_mtx);
      for (auto &p : g_profiles) {
        if (p.enabled && !p.match.empty() && base.find(p.match) != std::string::npos) return p;
      }
      return std::nullopt;
    }

    std::optional<app_profile_t> apply_for_exec(const std::string &exec_path) {
      auto m = find_match(exec_path);
      std::lock_guard<std::mutex> lock(g_mtx);
      if (!m) { g_active.reset(); return std::nullopt; }
      g_active = m->name;
      return m;
    }

    bool has_active_profile() {
      std::lock_guard<std::mutex> lock(g_mtx);
      return g_active.has_value();
    }

    std::optional<std::string> active_profile_name() {
      std::lock_guard<std::mutex> lock(g_mtx);
      return g_active;
    }
  }  // namespace app_profiler
}  // namespace solarflare
