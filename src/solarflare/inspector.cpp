/**
 * @file src/solarflare/inspector.cpp
 * @brief Definitions for the SolarFlare system inspector.
 */

// standard includes
#include <algorithm>
#include <cstdlib>
#include <fstream>
#include <sstream>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "logging.h"
#include "solarflare/codec_presets.h"
#include "solarflare/inspector.h"

namespace solarflare {

  namespace inspector {

    namespace {
      inspector_report_t g_cached {};

      std::string trim(const std::string &s) {
        auto b = s.find_first_not_of(" \t\r\n");
        if (b == std::string::npos) return {};
        auto e = s.find_last_not_of(" \t\r\n");
        return s.substr(b, e - b + 1);
      }

      // ponytail: read one slice of /proc/cpuinfo. The model line
      // on Ryzen doesn't say "zen", so we also parse the family/model
      // fields to disambiguate generations.
      cpu_info_t probe_cpu() {
        cpu_info_t cpu;
        std::ifstream f("/proc/cpuinfo");
        if (!f) return cpu;
        std::string line;
        while (std::getline(f, line)) {
          if (cpu.vendor.empty() && line.rfind("vendor_id", 0) == 0) {
            cpu.vendor = trim(line.substr(line.find(':') + 1));
          }
          if (cpu.model.empty() && line.rfind("model name", 0) == 0) {
            cpu.model = trim(line.substr(line.find(':') + 1));
            // Try to extract a microarch token.
            std::string m = cpu.model;
            for (auto &c : m) c = static_cast<char>(std::tolower(c));
            if (m.find("zen 4") != std::string::npos) cpu.microarch = "znver4";
            else if (m.find("zen 3") != std::string::npos) cpu.microarch = "znver3";
            else if (m.find("zen 2") != std::string::npos) cpu.microarch = "znver2";
            else if (m.find("zen") != std::string::npos) cpu.microarch = "znver1";
            else if (m.find("alder") != std::string::npos || m.find("raptor") != std::string::npos) cpu.microarch = "alderlake";
            else if (m.find("m1") != std::string::npos) cpu.microarch = "apple-m1";
            else if (m.find("m2") != std::string::npos) cpu.microarch = "apple-m2";
            else if (m.find("m3") != std::string::npos) cpu.microarch = "apple-m3";
          }
          if (line.rfind("processor", 0) == 0) {
            int n = 0;
            try { n = std::stoi(line.substr(line.find(':') + 1)); } catch (...) {}
            if (n >= cpu.threads) cpu.threads = n + 1;
          }
        }
        // Rough thread count fallback.
        if (cpu.cores == 0) cpu.cores = cpu.threads;
        return cpu;
      }

      gpu_info_t probe_gpu_linux() {
        gpu_info_t gpu;
        // Try the DRM subsystem first.
        std::error_code ec;
        for (auto it = std::filesystem::directory_iterator("/sys/class/drm", ec);
             !ec && it != std::filesystem::end(it, ec); it.increment(ec)) {
          std::ifstream vendor(it->path() / "device/vendor");
          std::ifstream device(it->path() / "device/device");
          std::string v;
          if (vendor && std::getline(vendor, v)) {
            if (v.find("0x10de") != std::string::npos) {
              gpu.vendor = gpu_vendor_e::nvidia;
              gpu.video_api = "nvenc";
              gpu.supports_hevc = true;
              gpu.supports_av1 = true;
            } else if (v.find("0x1002") != std::string::npos || v.find("0x1022") != std::string::npos) {
              gpu.vendor = gpu_vendor_e::amd;
              gpu.video_api = "vaapi";  // AMF for Windows; vaapi for Linux
              gpu.supports_hevc = true;
              gpu.supports_av1 = true;
            } else if (v.find("0x8086") != std::string::npos) {
              gpu.vendor = gpu_vendor_e::intel;
              gpu.video_api = "qsv";
              gpu.supports_hevc = true;
              gpu.supports_av1 = true;
            }
          }
          if (device && std::getline(device, gpu.name)) {
            gpu.name = trim(gpu.name);
            if (!gpu.name.empty()) break;
          }
        }
        return gpu;
      }

      display_server_e probe_display_linux() {
        // Wayland: WAYLAND_DISPLAY set or a wayland socket exists.
        if (std::getenv("WAYLAND_DISPLAY")) return display_server_e::wayland;
        if (std::filesystem::exists("/run/user/" + std::to_string(getuid()) + "/wayland-0")) {
          return display_server_e::wayland;
        }
        // X11: DISPLAY set and a unix socket exists.
        if (std::getenv("DISPLAY")) {
          return display_server_e::x11;
        }
        // KMS only: no DISPLAY, no wayland.
        if (std::filesystem::exists("/dev/dri/card0")) {
          return display_server_e::kms;
        }
        return display_server_e::headless;
      }

      capture_mode_e recommend_capture(inspector_report_t &r) {
        if (r.gpu.vendor == gpu_vendor_e::nvidia) return capture_mode_e::nvfbc;
#ifdef __linux__
        if (r.display_server == display_server_e::wayland) return capture_mode_e::wlgrab;
        if (r.display_server == display_server_e::x11) return capture_mode_e::x11;
        if (r.display_server == display_server_e::kms) return capture_mode_e::kmsgrab;
#endif
#ifdef _WIN32
        return capture_mode_e::wgc;
#endif
#ifdef __APPLE__
        return capture_mode_e::vt;
#endif
        return capture_mode_e::unknown;
      }

      void recommend_codec(inspector_report_t &r) {
        // Default: encoder matches the GPU vendor.
        r.recommended_encoder = "software";
        if (r.gpu.video_api == "nvenc") r.recommended_encoder = "nvenc";
        else if (r.gpu.video_api == "vaapi") r.recommended_encoder = "vaapi";
        else if (r.gpu.video_api == "qsv") r.recommended_encoder = "qsv";
#ifdef __APPLE__
        r.recommended_encoder = "vt";
#endif
        // Codec: AV1 if supported, else HEVC, else H.264.
        if (r.gpu.supports_av1) r.recommended_codec = "av1";
        else if (r.gpu.supports_hevc) r.recommended_codec = "hevc";
        else r.recommended_codec = "h264";
        // Pick a preset by name from codec_presets.
        std::string candidate = std::string(r.recommended_encoder == "nvenc" ? "NVENC Balanced " :
                                          r.recommended_encoder == "amf"    ? "AMF Balanced " :
                                          r.recommended_encoder == "vaapi"  ? "VAAPI Balanced" :
                                          r.recommended_encoder == "qsv"    ? "QuickSync Low Latency" :
                                          r.recommended_encoder == "vt"     ? "VideoToolbox HEVC" :
                                          r.recommended_encoder == "software" ? "Software x264 Balanced" : "Software x264 Balanced");
        auto enc_match = r.recommended_encoder;
        for (auto &p : codec_presets::all_presets()) {
          if (p.encoder != enc_match) continue;
          if (p.name.find(candidate) != std::string::npos) {
            r.recommended_preset = p.name;
            r.rationale.push_back("encoder matches GPU: " + enc_match);
            r.rationale.push_back("codec: " + r.recommended_codec);
            return;
          }
        }
        r.recommended_preset = candidate;
      }
    }  // namespace

    inspector_report_t probe() {
      inspector_report_t r;
#ifdef __linux__
      r.os_name = "Linux";
      r.cpu = probe_cpu();
      r.gpu = probe_gpu_linux();
      r.display_server = probe_display_linux();
      // Pull kernel version.
      std::ifstream uf("/proc/version");
      if (uf) {
        std::string v; std::getline(uf, v);
        auto sp = v.find(' ');
        if (sp != std::string::npos) v = v.substr(sp + 1);
        r.os_version = trim(v);
      }
      // VM / container warning.
      std::ifstream cm("/sys/class/dmi/id/product_name");
      if (cm) {
        std::string s; std::getline(cm, s);
        for (auto &needle : {"VirtualBox", "VMware", "QEMU", "KVM", "Virtual Machine"}) {
          if (s.find(needle) != std::string::npos) {
            r.warnings.push_back(std::string("running in a VM (") + needle + ") -- capture latency will be higher than bare metal");
            break;
          }
        }
      }
#elif _WIN32
      r.os_name = "Windows";
#elif __APPLE__
      r.os_name = "macOS";
#endif

      r.recommended_capture = recommend_capture(r);
      recommend_codec(r);
      if (!std::filesystem::exists("/dev/dri")) {
        r.warnings.push_back("no /dev/dri -- no hardware encode/decode available");
      }
      return r;
    }

    const inspector_report_t &cached() { return g_cached; }

    void init() {
      g_cached = probe();
      BOOST_LOG(info) << "SolarFlare inspector: probed. gpu=" << static_cast<int>(g_cached.gpu.vendor)
                      << " display=" << static_cast<int>(g_cached.display_server)
                      << " capture=" << static_cast<int>(g_cached.recommended_capture)
                      << " encoder=" << g_cached.recommended_encoder
                      << " codec=" << g_cached.recommended_codec
                      << " preset=" << g_cached.recommended_preset;
    }

    nlohmann::json to_json(const inspector_report_t &r) {
      nlohmann::json j;
      j["os"] = { {"name", r.os_name}, {"version", r.os_version} };
      j["cpu"] = {
        {"vendor", r.cpu.vendor}, {"model", r.cpu.model},
        {"microarch", r.cpu.microarch}, {"cores", r.cpu.cores}, {"threads", r.cpu.threads}
      };
      j["gpu"] = {
        {"vendor", static_cast<int>(r.gpu.vendor)},
        {"name", r.gpu.name}, {"driver", r.gpu.driver}, {"video_api", r.gpu.video_api},
        {"supports_hevc", r.gpu.supports_hevc}, {"supports_av1", r.gpu.supports_av1}
      };
      j["display_server"] = static_cast<int>(r.display_server);
      j["recommended"] = {
        {"capture", static_cast<int>(r.recommended_capture)},
        {"codec", r.recommended_codec}, {"encoder", r.recommended_encoder},
        {"preset", r.recommended_preset}
      };
      j["warnings"] = r.warnings;
      j["rationale"] = r.rationale;
      return j;
    }
  }  // namespace inspector
}  // namespace solarflare
