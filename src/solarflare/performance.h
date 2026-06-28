/**
 * @file src/solarflare/performance.h
 * @brief Declarations for the SolarFlare "pre-stream performance prep" mode.
 *
 * One click before you start streaming should do four things upstream
 * Sunshine can't:
 *
 *   1. Raise HID polling from the kernel 125Hz default to 1000Hz,
 *      so a 1000Hz gaming mouse actually delivers every sample.
 *   2. Set the GPU to its highest performance state (NVIDIA
 *      persistence + perf-cap, AMD power_dpm, Intel min_freq).
 *   3. Inhibit display blanking / sleep for the duration of the
 *      stream so the host monitor doesn't blank when the client
 *      is the only thing you're looking at.
 *   4. Mark the streaming UDP socket with DSCP EF so a QoS-aware
 *      router puts it ahead of bulk traffic on the same link.
 *
 * Each prep step is gated on its own config flag so a user can
 * disable the ones they don't want without rebuilding.
 */
#pragma once

// standard includes
#include <atomic>
#include <cstdint>
#include <string>
#include <vector>

namespace solarflare {

  struct perf_cfg_t {
    bool enabled = true;
    bool boost_hid_polling = true;       ///< write 1000Hz to /sys/module/usbhid/parameters/poll
    bool set_gpu_perf = true;            ///< platform-specific perf hint
    bool inhibit_blanking = true;        ///< inhibit sleep during stream
    bool mark_qos = true;                ///< set DSCP on streaming socket
    std::uint32_t hid_target_hz = 1000;  ///< target HID polling rate
  };

  struct perf_step_t {
    std::string name;
    bool applied = false;
    std::string detail;  ///< human-readable: "set /sys/module/usbhid/parameters/poll = 1000"
  };

  struct perf_snap_t {
    bool active = false;          ///< true between apply() and revert()
    std::vector<perf_step_t> steps;
    perf_cfg_t cfg;
    std::int64_t started_at_ns = 0;
  };

  namespace performance {
    bool init(const perf_cfg_t &cfg);
    void shutdown();

    /** @brief Apply all enabled steps. Idempotent. */
    bool apply();

    /** @brief Revert all applied steps. Idempotent. */
    void revert();

    /** @brief Snapshot of current state. */
    perf_snap_t snapshot();
  }  // namespace performance
}  // namespace solarflare
