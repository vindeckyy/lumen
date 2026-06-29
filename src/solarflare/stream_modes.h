/**
 * @file src/solarflare/stream_modes.h
 * @brief SolarFlare streaming modes — pre-baked config bundles that
 *        flip many knobs at once.
 *
 * ponytail: a streaming mode is a (name, description, kv) tuple
 * where `kv` touches multiple Sunshine config groups (stream::,
 * video::, input::, plus the SolarFlare tunables). One click on
 * the dashboard applies a mode; the user can override individual
 * keys in the regular config UI afterwards.
 *
 * Unlike codec presets (which only touch the encoder), modes
 * reconfigure the entire pipe: packet sizes, FEC, input batching,
 * HDR handling, ALLM triggers.
 */
#pragma once

// standard includes
#include <string>
#include <vector>

namespace solarflare {

  /**
   * @brief A pre-baked streaming mode.
   */
  struct stream_mode_t {
    std::string name;
    std::string description;
    /**
     * @brief Key/value pairs to apply. Each pair runs through
     * the upstream `apply_config` parser, so any value the
     * Sunshine config accepts is accepted here.
     */
    std::vector<std::pair<std::string, std::string>> kv;
  };

  namespace stream_modes {
    /** @brief List of shipped modes. */
    const std::vector<stream_mode_t> &all();

    /** @brief Look up a single mode by name. */
    const stream_mode_t *find(const std::string &name);

    /** @brief Apply a mode's keys to the user's sunshine.conf. */
    bool apply(const std::string &name);

    /** @brief Render all modes as JSON for the UI. */
    nlohmann::json all_json();
  }  // namespace stream_modes
}  // namespace solarflare
