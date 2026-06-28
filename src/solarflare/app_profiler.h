/**
 * @file src/solarflare/app_profiler.h
 * @brief SolarFlare app profiler: per-application config overrides.
 *
 * ponytail: a JSON file with N profiles; matching is a substring of
 * the executable basename. The host runtime calls `apply_for_exec`
 * when an app launches; the dashboard reads/writes the JSON directly.
 */
#pragma once

// standard includes
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

namespace solarflare {

  struct app_profile_t {
    std::string name;            // display name
    std::string match;           // substring of exec basename
    int priority = 100;          // lower = applied first
    bool enabled = true;
    std::optional<std::uint32_t> fps, width, height, bitrate_kbps;
    std::optional<std::string> encoder, codec;
  };

  struct app_profiler_cfg_t {
    bool enabled = true;
    std::filesystem::path profiles_path;  // ponytail: default <appdata>/solarflare/profiles.json
  };

  namespace app_profiler {
    bool init(const app_profiler_cfg_t &cfg);
    void shutdown();
    void reload();
    void save();

    std::vector<app_profile_t> list();
    void upsert(const app_profile_t &p);
    bool remove(const std::string &name);

    std::optional<app_profile_t> find_match(const std::string &exec_path);
    std::optional<app_profile_t> apply_for_exec(const std::string &exec_path);
    bool has_active_profile();
    std::optional<std::string> active_profile_name();
  }  // namespace app_profiler
}  // namespace solarflare
