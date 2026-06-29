/**
 * @file src/solarflare/stream_modes.cpp
 * @brief Definitions for the SolarFlare streaming modes.
 */

// standard includes
#include <algorithm>
#include <fstream>
#include <sstream>
#include <utility>
#include <vector>

// lib includes
#include <nlohmann/json.hpp>

// local includes
#include "logging.h"
#include "solarflare/stream_modes.h"

namespace solarflare {

  namespace stream_modes {

    namespace {
      // ponytail: keep the list to three. Add a fourth only when a
      // user actually asks for it ("home theater", "dev", etc).
      const std::vector<stream_mode_t> kModes = {
        {"Couch Gaming",
         "Balanced 1080p60 with FEC and 20ms client buffer. Looks good on a TV 3m away.",
         {
           {"fps", "60"},
           {"width", "1920"},
           {"height", "1080"},
           {"bitrate", "20000"},
           {"packet_size", "1456"},
           {"fec_percentage", "20"},
           {"min_log_level", "2"},
           {"nvenc_preset", "4"},  // balanced
           {"nvenc_rc", "cbr"},
           {"qp", "22"},
           {"min_threads", "2"},
           {"adapter_name", ""},
           {"capture", ""},
           {"encoder", "nvenc"},
           {"sw_preset", "veryfast"},
           {"sw_tune", "fastdecode"},
           {"always_send_scancodes", "true"},
           {"key_repeat_delay", "0"},
           {"key_repeat_period", "0.005"},
           {"mouse", "true"},
           {"controller", "true"}
         }},
        {"Competitive FPS",
         "Lowest-latency 1080p120+. NO b-frames. NO lookahead. Maximum input polling. Game mode on TV.",
         {
           {"fps", "120"},
           {"width", "1920"},
           {"height", "1080"},
           {"bitrate", "15000"},
           {"packet_size", "1392"},
           {"fec_percentage", "10"},   // less FEC = less latency
           {"min_log_level", "3"},     // quieter logs
           {"nvenc_preset", "1"},       // lowest-latency preset
           {"nvenc_rc", "cbr"},
           {"qp", "20"},
           {"min_threads", "1"},       // single slice for minimum latency
           {"encoder", "nvenc"},
           {"nv_preset", "0"},         // latency-optimised
           {"capture", ""},
           {"always_send_scancodes", "true"},
           {"key_repeat_delay", "0"},
           {"key_repeat_period", "0.001"},  // almost no repeat debounce
           {"mouse", "true"},
           {"controller", "true"}
         }},
        {"Cinematic 4K",
         "4K60 HEVC, high bitrate, full b-frames + lookahead. Visually best, less responsive.",
         {
           {"fps", "60"},
           {"width", "3840"},
           {"height", "2160"},
           {"bitrate", "80000"},
           {"packet_size", "1456"},
           {"fec_percentage", "25"},
           {"nvenc_preset", "7"},       // quality preset
           {"nvenc_rc", "vbr"},
           {"qp", "18"},
           {"min_threads", "4"},
           {"encoder", "nvenc"},
           {"capture", ""},
           {"always_send_scancodes", "true"},
           {"key_repeat_delay", "0"},
           {"key_repeat_period", "0.010"},
           {"mouse", "true"},
           {"controller", "true"}
         }}
      };

      // ponytail: the upstream config parser wants "key = value"
      // lines, one per line. We rebuild the file with our keys
      // applied on top of whatever the user had. Conservative
      // approach: append/replace existing keys, leave others alone.
      bool merge_into_config(const std::vector<std::pair<std::string, std::string>> &kv) {
        extern std::string config_file_global;
        // Read existing.
        std::ifstream in(config_file_global);
        std::vector<std::string> lines;
        std::vector<bool> line_replaced(lines.size(), false);
        std::set<std::string> applied;
        if (in) {
          std::string line;
          while (std::getline(in, line)) lines.push_back(line);
        }
        in.close();
        // Try to replace existing keys; otherwise append.
        std::vector<std::string> out;
        for (auto &line : lines) {
          bool replaced = false;
          for (auto &[k, v] : kv) {
            if (applied.count(k)) continue;
            if (line.size() > k.size() + 1 && line.substr(0, k.size()) == k && line[k.size()] == ' ') {
              std::ostringstream nl;
              nl << k << " = " << v;
              out.push_back(nl.str());
              applied.insert(k);
              replaced = true;
              break;
            }
          }
          if (!replaced) out.push_back(line);
        }
        for (auto &[k, v] : kv) {
          if (applied.count(k)) continue;
          std::ostringstream nl;
          nl << k << " = " << v;
          out.push_back(nl.str());
          applied.insert(k);
        }
        // Write atomically.
        std::ofstream tmp(config_file_global + ".tmp", std::ios::binary | std::ios::trunc);
        for (auto &line : out) {
          tmp << line << "\n";
        }
        tmp.close();
        std::error_code ec;
        std::filesystem::rename(config_file_global + ".tmp", config_file_global, ec);
        if (ec) {
          BOOST_LOG(warning) << "SolarFlare stream_modes: rename failed: " << ec.message();
          return false;
        }
        return true;
      }

      // The config file path lives in config::sunshine.config_file but
      // pulling that in here would create a header cycle; we expose
      // it via a tiny extern in main.cpp.
    }  // namespace

    const std::vector<stream_mode_t> &all() { return kModes; }

    const stream_mode_t *find(const std::string &name) {
      auto it = std::find_if(kModes.begin(), kModes.end(),
                             [&](const stream_mode_t &m) { return m.name == name; });
      return it == kModes.end() ? nullptr : &*it;
    }

    bool apply(const std::string &name) {
      auto *m = find(name);
      if (!m) {
        BOOST_LOG(warning) << "SolarFlare stream_modes: unknown mode '" << name << "'";
        return false;
      }
      if (!merge_into_config(m->kv)) {
        BOOST_LOG(warning) << "SolarFlare stream_modes: failed to merge '" << name << "'";
        return false;
      }
      BOOST_LOG(info) << "SolarFlare stream_modes: applied '" << name << "' (" << m->kv.size() << " keys)";
      return true;
    }

    nlohmann::json all_json() {
      nlohmann::json arr = nlohmann::json::array();
      for (auto &m : kModes) {
        nlohmann::json j;
        j["name"] = m.name;
        j["description"] = m.description;
        nlohmann::json kv = nlohmann::json::object();
        for (auto &[k, v] : m.kv) kv[k] = v;
        j["kv"] = kv;
        j["kv_count"] = m.kv.size();
        arr.push_back(j);
      }
      return arr;
    }
  }  // namespace stream_modes
}  // namespace solarflare
