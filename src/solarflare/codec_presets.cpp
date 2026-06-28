/**
 * @file src/solarflare/codec_presets.cpp
 * @brief Definitions for the SolarFlare codec quality presets.
 */

// standard includes
#include <algorithm>
#include <utility>
#include <vector>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "solarflare/codec_presets.h"

namespace solarflare {

  namespace codec_presets {

    namespace {
      // ponytail: keep the list short and obvious. The four presets
      // per encoder below are the ones a user would actually pick.
      // Add more when a real user asks for one -- not "while we're
      // here".
      const std::vector<preset_t> kPresets = {
        // ---- NVENC (NVIDIA) ----
        {"NVENC Low Latency 1080p60", "nvenc", "h264",
         "P1, no B-frames, zerolatency -- the absolute floor on encode latency.",
         {{"nvenc_preset", "1"}, {"nvenc_rc", "cbr"}, {"min_threads", "1"}, {"qp", "20"}}},
        {"NVENC Balanced 1440p120", "nvenc", "hevc",
         "P4, 2 B-frames, lookahead on. Good middle ground for 1440p.",
         {{"nvenc_preset", "4"}, {"nvenc_rc", "cbr"}, {"min_threads", "2"}, {"qp", "22"}}},
        {"NVENC Quality 4K60", "nvenc", "hevc",
         "P7, 4 B-frames, lookahead 40, two-pass full. Visual quality over latency.",
         {{"nvenc_preset", "7"}, {"nvenc_rc", "vbr"}, {"min_threads", "4"}, {"qp", "18"}}},
        {"NVENC AV1 4K120", "nvenc", "av1",
         "P7 AV1 for Ada+ GPUs. Best compression-per-bit at high resolutions.",
         {{"nvenc_preset", "7"}, {"nvenc_rc", "vbr"}, {"min_threads", "4"}, {"qp", "20"}}},

        // ---- AMF (AMD) ----
        {"AMF Low Latency 1080p60", "amf", "h264",
         "AMF speed preset, VBR, no preanalysis. Good for twitch gameplay.",
         {{"amd_usage_h264", "0"}, {"amd_rc_h264", "1"}, {"amd_enforce_hrd", "1"}, {"amd_quality_h264", "0"}, {"amd_vbaq", "0"}}},
        {"AMF Balanced 1440p", "amf", "hevc",
         "Balanced preset, VBR, preanalysis on.",
         {{"amd_usage_hevc", "1"}, {"amd_rc_hevc", "1"}, {"amd_enforce_hrd", "1"}, {"amd_quality_hevc", "1"}, {"amd_preanalysis", "1"}, {"amd_vbaq", "1"}}},
        {"AMF Quality 4K", "amf", "av1",
         "AV1 quality preset, lookahead on. Slower encode, smaller bitrate.",
         {{"amd_usage_av1", "2"}, {"amd_rc_av1", "2"}, {"amd_quality_av1", "2"}, {"amd_preanalysis", "1"}, {"amd_vbaq", "1"}}},

        // ---- VAAPI (Intel / AMD open source) ----
        {"VAAPI Low Latency", "vaapi", "h264",
         "VAAPI low-latency CQP, target 20.",
         {{"vaapi_strict_rc_buffer", "1"}}},
        {"VAAPI Balanced", "vaapi", "hevc",
         "VAAPI balanced CQP, target 22.",
         {{"vaapi_strict_rc_buffer", "0"}}},

        // ---- Software (x264 / x265 / SVT-AV1 / kvazaar) ----
        {"Software x264 Low Latency", "software", "h264",
         "x264 preset ultrafast, tune zerolatency, CABAC.",
         {{"sw_preset", "ultrafast"}, {"sw_tune", "zerolatency"}}},
        {"Software x264 Balanced", "software", "h264",
         "x264 preset veryfast, tune fastdecode. Reasonable CPU cost.",
         {{"sw_preset", "veryfast"}, {"sw_tune", "fastdecode"}}},
        {"Software x265 Quality", "software", "hevc",
         "x265 preset medium, no tune. Best HEVC per-bit at high CPU cost.",
         {{"sw_preset", "medium"}}},
        {"Software SVT-AV1 6", "software", "av1",
         "SVT-AV1 preset 6. Best open-source AV1 quality/speed tradeoff.",
         {{"svtav1_preset", "6"}}},

        // ---- VideoToolbox (macOS) ----
        {"VideoToolbox Low Latency", "vt", "h264",
         "VT realtime on, software encoders disallowed.",
         {{"vt_realtime", "1"}, {"vt_allow_sw", "0"}}},
        {"VideoToolbox HEVC", "vt", "hevc",
         "VT HEVC realtime, software fallback off.",
         {{"vt_realtime", "1"}, {"vt_allow_sw", "0"}}},

        // ---- QuickSync (Intel) ----
        {"QuickSync Low Latency", "qsv", "h264",
         "QSV preset veryfast, CAVLC off.",
         {{"qsv_preset", "6"}, {"qsv_cavlc", "0"}}},
        {"QuickSync HEVC Slow", "qsv", "hevc",
         "QSV preset slower for HEVC. Better compression at the cost of CPU.",
         {{"qsv_preset", "3"}, {"qsv_slow_hevc", "1"}}},

        // ---- Vulkan ----
        {"Vulkan Low Latency", "vulkan", "h264",
         "Vulkan encode, low-latency rate-control mode.",
         {{"vk_rc_mode", "2"}, {"vk_tune", "2"}}},
        {"Vulkan Quality", "vulkan", "hevc",
         "Vulkan encode, quality rate-control mode.",
         {{"vk_rc_mode", "4"}, {"vk_tune", "1"}}}
      };
    }  // namespace

    const std::vector<preset_t> &all_presets() { return kPresets; }

    std::vector<preset_t> presets_for_encoder(const std::string &encoder) {
      std::vector<preset_t> out;
      for (auto &p : kPresets) {
        if (p.encoder == encoder) out.push_back(p);
      }
      return out;
    }

    nlohmann::json all_presets_json() {
      nlohmann::json arr = nlohmann::json::array();
      for (auto &p : kPresets) {
        nlohmann::json j;
        j["name"] = p.name;
        j["encoder"] = p.encoder;
        j["codec"] = p.codec;
        j["description"] = p.description;
        nlohmann::json kv = nlohmann::json::object();
        for (auto &[k, v] : p.kv) kv[k] = v;
        j["kv"] = kv;
        arr.push_back(j);
      }
      return arr;
    }

    const preset_t *find(const std::string &name) {
      auto it = std::find_if(kPresets.begin(), kPresets.end(),
                             [&](const preset_t &p) { return p.name == name; });
      return it == kPresets.end() ? nullptr : &*it;
    }
  }  // namespace codec_presets
}  // namespace solarflare
