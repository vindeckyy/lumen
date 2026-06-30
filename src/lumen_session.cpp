/**
 * @file src/lumen_session.cpp
 * @brief Implementation of the Lumen per-session metrics registry.
 */

// standard includes
#include <random>
#include <sstream>
#include <iomanip>

// local includes
#include "lumen_session.h"

namespace lumen {
  namespace session {

    namespace {
      std::mutex g_mu;
      std::unordered_map<std::string, Metrics> g_sessions;

      std::string gen_id() {
        // std::random_device + uniform distribution is the stdlib fix;
        // if collisions ever matter, swap to uuid_v4 from libuuid.
        static thread_local std::mt19937_64 rng {std::random_device{}()};
        std::uniform_int_distribution<uint64_t> dist;
        std::ostringstream os;
        os << std::hex << std::setw(16) << std::setfill('0') << dist(rng)
           << std::setw(16) << std::setfill('0') << dist(rng);
        return os.str();
      }

      int64_t to_ms(std::chrono::system_clock::time_point tp) {
        return std::chrono::duration_cast<std::chrono::milliseconds>(tp.time_since_epoch()).count();
      }

      // One helper per metric keeps render() trivially auditable.
      void emit(std::ostringstream &out, const char *k, int64_t v) {
        out << "  \"" << k << "\": " << v;
      }
      void emit_d(std::ostringstream &out, const char *k, double v) {
        out.precision(3);
        out << "  \"" << k << "\": " << v;
      }

      std::string render(const Metrics &m) {
        std::ostringstream out;
        out << "{\n";
        out << "  \"id\": \"" << m.id << "\",\n";
        out << "  \"app_name\": \"" << m.app_name << "\",\n";
        out << "  \"client_name\": \"" << m.client_name << "\",\n";
        emit(out, "started_at_ms"); out << ": " << to_ms(m.started_at) << ",\n";
        emit_d(out, "encoder_fps"); out << ",\n";
        emit_d(out, "target_fps"); out << ",\n";
        emit(out, "bitrate_kbps"); out << ",\n";
        emit(out, "rtt_ms"); out << ",\n";
        emit_d(out, "encode_ms"); out << ",\n";
        emit(out, "frames_sent"); out << ",\n";
        emit(out, "frames_dropped"); out << "\n";
        out << "}";
        return out.str();
      }
    }  // namespace

    std::string start(const std::string &app_name, const std::string &client_name) {
      std::lock_guard<std::mutex> lock(g_mu);
      Metrics m;
      m.id = gen_id();
      m.app_name = app_name;
      m.client_name = client_name;
      m.started_at = std::chrono::system_clock::now();
      m.started_at_ms = to_ms(m.started_at);
      g_sessions.emplace(m.id, std::move(m));
      return g_sessions.find(m.id)->first;
    }

    bool record_sample(const std::string &id, const Metrics &update) {
      std::lock_guard<std::mutex> lock(g_mu);
      auto it = g_sessions.find(id);
      if (it == g_sessions.end()) return false;
      // Preserve id + started_at + names; overwrite the numeric fields.
      Metrics &m = it->second;
      m.encoder_fps   = update.encoder_fps;
      m.target_fps    = update.target_fps;
      m.bitrate_kbps  = update.bitrate_kbps;
      m.rtt_ms        = update.rtt_ms;
      m.encode_ms     = update.encode_ms;
      m.frames_sent   = update.frames_sent;
      m.frames_dropped = update.frames_dropped;
      return true;
    }

    void stop(const std::string &id) {
      std::lock_guard<std::mutex> lock(g_mu);
      g_sessions.erase(id);
    }

    std::string list_active_json() {
      std::lock_guard<std::mutex> lock(g_mu);
      std::ostringstream out;
      out << "[";
      bool first = true;
      for (const auto &kv : g_sessions) {
        if (!first) out << ",";
        first = false;
        out << "\n" << render(kv.second);
      }
      if (!g_sessions.empty()) out << "\n";
      out << "]";
      return out.str();
    }

    std::string get_json(const std::string &id) {
      std::lock_guard<std::mutex> lock(g_mu);
      auto it = g_sessions.find(id);
      if (it == g_sessions.end()) return "";
      return render(it->second);
    }

  }  // namespace session
}  // namespace lumen