/**
 * @file src/solarflare/latency_budget.cpp
 * @brief Definitions for the latency budget module.
 */

// local includes
#include "solarflare/latency_budget.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace latency_budget {

    latency_budget_t from_snapshot(
      const telemetry::snapshot_t &snap,
      std::uint32_t decode_us,
      std::uint32_t display_us) {
      latency_budget_t b;
      b.capture_us = snap.latency[STAGE_CAPTURE].avg_us;
      b.color_convert_us = snap.latency[STAGE_COLOR_CONVERT].avg_us;
      b.encode_us = snap.latency[STAGE_ENCODE].avg_us;
      b.pacer_us = snap.latency[STAGE_PACER].avg_us;
      b.send_us = snap.latency[STAGE_SEND].avg_us;
      b.network_rtt_us = snap.latency[STAGE_NETWORK_RTT].avg_us;
      b.decode_us = decode_us;
      b.display_us = display_us;
      if (snap.session.active && snap.session.fps > 0) {
        b.frame_budget_us = static_cast<std::uint32_t>(1000000 / snap.session.fps);
      }
      return b;
    }

    nlohmann::json to_json(const latency_budget_t &b) {
      nlohmann::json j;
      j["server"] = {
        {"capture_us", b.capture_us},
        {"color_convert_us", b.color_convert_us},
        {"encode_us", b.encode_us},
        {"pacer_us", b.pacer_us},
        {"send_us", b.send_us},
        {"total_us", b.server_total_us()}
      };
      j["client"] = {
        {"network_one_way_us", b.network_rtt_us / 2},
        {"decode_us", b.decode_us},
        {"display_us", b.display_us},
        {"total_us", b.client_total_us()}
      };
      j["end_to_end_us"] = b.end_to_end_us();
      j["frame_budget_us"] = b.frame_budget_us;
      j["fits_in_budget"] = b.fits_in_budget();
      return j;
    }
  }  // namespace latency_budget
}  // namespace solarflare
