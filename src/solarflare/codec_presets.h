/**
 * @file src/solarflare/codec_presets.h
 * @brief SolarFlare named codec quality presets.
 *
 * ponytail: a preset is just a (key, value) set the host can apply
 * to the upstream config struct when the user picks one. We ship a
 * short list of named presets per encoder; the web UI lists them and
 * the user clicks to apply. No migration, no DB, no per-game
 * intelligence -- just the obvious names a user would type.
 */
#pragma once

// standard includes
#include <string>
#include <vector>

// lib includes
#include <nlohmann/json.hpp>

namespace solarflare {

  namespace codec_presets {

    /**
     * @brief One named preset: maps config keys to string values.
     *
     * We use stringly-typed values because the upstream config
     * parser is stringly-typed and we don't want to fork the
     * parser. Numbers come back as strings; ints are written
     * unquoted, strings quoted; bools as "true"/"false".
     */
    struct preset_t {
      std::string name;        ///< Display name, e.g. "Low latency 1080p60".
      std::string encoder;     ///< "nvenc" | "amf" | "vaapi" | "software" | "vt" | "qsv" | "vulkan".
      std::string codec;       ///< "h264" | "hevc" | "av1".
      std::string description; ///< One-line description for the UI.
      std::vector<std::pair<std::string, std::string>> kv;
    };

    /**
     * @brief Return every preset shipped with SolarFlare.
     */
    const std::vector<preset_t> &all_presets();

    /**
     * @brief Return presets that match a given encoder.
     */
    std::vector<preset_t> presets_for_encoder(const std::string &encoder);

    /**
     * @brief Render all presets as JSON for the web UI.
     */
    nlohmann::json all_presets_json();

    /**
     * @brief Look up a single preset by name.
     */
    const preset_t *find(const std::string &name);

  }  // namespace codec_presets
}  // namespace solarflare
