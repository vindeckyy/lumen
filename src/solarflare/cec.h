/**
 * @file src/solarflare/cec.h
 * @brief Declarations for CEC ALLM (Auto Low Latency Mode) control.
 *
 * When a Moonlight client starts streaming, the right thing for a
 * connected TV is to switch into game mode. ALLM is the modern
 * standard (HDMI 2.1) and most TVs from 2019+ support it. We send the
 * "Set Stream Path" CEC message via cec-client on Linux; macOS
 * doesn't expose CEC, and Windows needs a different library.
 *
 * If `cec-client` isn't installed, the call silently returns
 * "unsupported" and the user sees that in the snapshot.
 */
#pragma once

// standard includes
#include <string>

namespace solarflare {

  enum class allm_state_e {
    unknown,     ///< haven't tried yet
    unsupported, ///< no CEC adapter / library
    on,          ///< game mode signalled to TV
    off,         ///< restored TV to non-game state
    failed,      ///< tried, TV / adapter rejected
  };

  struct cec_snap_t {
    allm_state_e state = allm_state_e::unknown;
    std::string detail;        ///< "sent cec-client -s on", "no cec-client", ...
    std::string tv_vendor;     ///< from CEC EDID if available
  };

  namespace allm {
    /** @brief Ask the connected TV to enter game mode. */
    bool enable();

    /** @brief Tell the TV to leave game mode (best-effort). */
    bool disable();

    /** @brief Snapshot of last attempt. */
    cec_snap_t snapshot();
  }  // namespace allm
}  // namespace solarflare
