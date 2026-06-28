/**
 * @file src/solarflare/latency_budget.h
 * @brief Declarations for the latency budget breakdown.
 *
 * When the dashboard says "your input-to-photon latency is 47 ms",
 * the next question is always "where?". The latency budget answers
 * that by splitting a target 16.67 ms (60 fps) frame budget across
 * the pipeline stages, plus showing the network round-trip and the
 * estimated client-side decode+display latency.
 *
 * ponytail: it's a function of `telemetry::snapshot_t` plus a
 * couple of configurable estimates. No background thread.
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "solarflare/telemetry.h"

namespace solarflare {

  struct latency_budget_t {
    // Per-stage measured latency (microseconds).
    std::uint32_t capture_us = 0;
    std::uint32_t color_convert_us = 0;
    std::uint32_t encode_us = 0;
    std::uint32_t pacer_us = 0;
    std::uint32_t send_us = 0;
    std::uint32_t network_rtt_us = 0;
    // Estimated fixed overheads.
    std::uint32_t decode_us = 0;        ///< client-side HEVC/AV1 decode estimate
    std::uint32_t display_us = 0;       ///< client scanout + TV input lag estimate
    // Totals.
    std::uint32_t server_total_us() const {
      return capture_us + color_convert_us + encode_us + pacer_us + send_us;
    }
    std::uint32_t client_total_us() const {
      return network_rtt_us / 2 + decode_us + display_us;
    }
    std::uint32_t end_to_end_us() const {
      return server_total_us() + client_total_us();
    }
    // Frame budget for the active session (e.g. 16667 for 60 fps).
    std::uint32_t frame_budget_us = 16667;
    bool fits_in_budget() const { return server_total_us() <= frame_budget_us; }
  };

  namespace latency_budget {
    /**
     * @brief Build a budget from a telemetry snapshot.
     *
     * The decode_us and display_us are user-tunable estimates; the
     * defaults match a typical 2024 TV in game mode + the Moonlight
     * client doing software decode.
     */
    latency_budget_t from_snapshot(
      const snapshot_t &snap,
      std::uint32_t decode_us = 4000,
      std::uint32_t display_us = 8000);

    nlohmann::json to_json(const latency_budget_t &b);
  }  // namespace latency_budget
}  // namespace solarflare
