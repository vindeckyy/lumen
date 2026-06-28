/**
 * @file src/solarflare/inspector.h
 * @brief SolarFlare system inspector: detect host capabilities and
 *        recommend capture mode + codec.
 *
 * ponytail: one struct of plain data, populated at init time from
 * /proc/cpuinfo, /sys/class/drm, the NVML probe, and platform.h.
 * No background thread -- the user wants the snapshot once when
 * the dashboard first loads.
 */
#pragma once

// standard includes
#include <string>
#include <vector>

// lib includes
#include <nlohmann/json.hpp>

namespace solarflare {

  enum class display_server_e {
    unknown,
    wayland,
    x11,
    kms,        // direct KMS/DMA-BUF, no compositor
    headless,   // no display
  };

  enum class capture_mode_e {
    unknown,
    nvfbc,      // NVIDIA frame buffer capture
    kmsgrab,    // DRM/KMS direct
    wlgrab,     // Wayland screencopy
    x11,        // X11 SHM
    wgc,        // Windows.Graphics.Capture
    dxgi,       // D3D11 desktop duplication
    vt,         // macOS VideoToolbox capture
  };

  enum class gpu_vendor_e {
    none,
    nvidia,
    amd,
    intel,
    apple,
    unknown,
  };

  struct cpu_info_t {
    std::string vendor;
    std::string model;
    std::string microarch;  ///< "znver2", "alderlake", "apple-m1", etc.
    int cores = 0;
    int threads = 0;
  };

  struct gpu_info_t {
    gpu_vendor_e vendor = gpu_vendor_e::none;
    std::string name;
    std::string driver;
    std::string video_api;  ///< "nvenc", "vaapi", "amf", "videotoolbox"
    bool supports_hevc = false;
    bool supports_av1 = false;
  };

  struct inspector_report_t {
    display_server_e display_server = display_server_e::unknown;
    capture_mode_e recommended_capture = capture_mode_e::unknown;
    cpu_info_t cpu;
    gpu_info_t gpu;
    std::string os_name;          ///< "Linux", "Windows", "macOS"
    std::string os_version;       ///< "6.12.0", "10.0.22631", "14.5"
    std::string kernel_args;      ///< relevant cmdline flags if available
    std::vector<std::string> warnings;  ///< e.g. "running in a VM", "no /dev/dri"

    // Recommendations derived from the above.
    std::string recommended_codec;       ///< "hevc", "h264", "av1"
    std::string recommended_encoder;     ///< "nvenc", "amf", "vaapi", "software"
    std::string recommended_preset;      ///< name from codec_presets
    std::vector<std::string> rationale;  ///< one line per reason
  };

  namespace inspector {
    /** @brief Run all probes and return a populated report. */
    inspector_report_t probe();

    /** @brief Last cached probe result (init() runs one at startup). */
    const inspector_report_t &cached();

    /** @brief Run a probe at startup so the first web UI load is fast. */
    void init();

    /** @brief Render the report as JSON for the API. */
    nlohmann::json to_json(const inspector_report_t &r);
  }  // namespace inspector
}  // namespace solarflare
