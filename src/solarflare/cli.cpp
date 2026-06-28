/**
 * @file src/solarflare/cli.cpp
 * @brief Definitions for `solarctl`.
 *
 * ponytail: each subcommand is a 5-line dispatcher. The work happens
 * in the underlying modules; this file just chooses between them and
 * formats the output.
 */

// standard includes
#include <cstring>
#include <iomanip>
#include <iostream>
#include <string>
#include <vector>

// local includes
#include "config.h"
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/cli.h"
#include "solarflare/codec_presets.h"
#include "solarflare/health_monitor.h"
#include "solarflare/network_probe.h"
#include "solarflare/session_recorder.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace cli {
    namespace {
      void print_table(const std::vector<std::vector<std::string>> &rows) {
        std::vector<std::size_t> widths(rows[0].size(), 0);
        for (auto &r : rows)
          for (std::size_t i = 0; i < r.size() && i < widths.size(); ++i)
            widths[i] = std::max(widths[i], r[i].size());
        for (auto &r : rows) {
          for (std::size_t i = 0; i < r.size() && i < widths.size(); ++i)
            std::cout << std::left << std::setw(static_cast<int>(widths[i]) + 2) << r[i];
          std::cout << "\n";
        }
      }

      int status() {
        auto snap = telemetry::snapshot();
        std::cout << "SolarFlare status\n=================\n";
        std::cout << "Session active: " << (snap.session.active ? "yes" : "no") << "\n";
        if (snap.session.active) {
          std::cout << "  client: " << snap.session.client_name << "\n";
          std::cout << "  app:    " << snap.session.client_app << "\n";
          std::cout << "  " << snap.session.width << "x" << snap.session.height
                    << "@" << snap.session.fps << " " << snap.session.bitrate_kbps << " kbps\n";
        }
        std::cout << "Frames: " << snap.frames << ", Bytes: " << snap.bytes_sent
                  << ", Lost: " << snap.packets_lost << "\n";
        return 0;
      }

      int telemetry_cli() {
        auto s = telemetry::snapshot();
        std::vector<std::vector<std::string>> rows {{"stage", "min_us", "max_us", "avg_us", "n"}};
        for (std::size_t i = 0; i < stage_count; ++i) {
          rows.push_back({
            stage_name(static_cast<pipeline_stage>(i)),
            std::to_string(s.latency[i].min_us),
            std::to_string(s.latency[i].max_us),
            std::to_string(static_cast<int>(s.latency[i].avg_us)),
            std::to_string(s.latency[i].n)
          });
        }
        print_table(rows);
        return 0;
      }

      int health_cli() {
        auto h = health_monitor::snapshot();
        std::cout << "resources:\n";
        for (auto &r : h.resources) std::cout << "  " << r.name << " = " << r.value << "\n";
        std::cout << "recent alerts (" << h.recent_alerts.size() << "):\n";
        for (auto &a : h.recent_alerts)
          std::cout << "  [" << static_cast<int>(a.severity) << "] " << a.resource << ": " << a.message << "\n";
        return 0;
      }

      int sessions_cli(int argc, char **argv) {
        std::size_t n = (argc >= 1 && argv[0]) ? static_cast<std::size_t>(std::atoi(argv[0])) : 10;
        session_cfg_t cfg;
        session_recorder::init(cfg);
        auto recs = session_recorder::list(n);
        std::cout << "sessions (newest first, max " << n << "):\n";
        for (auto &r : recs) {
          std::cout << "  " << r.id << "  " << r.client_name << "/" << r.client_app
                    << "  " << r.width << "x" << r.height << "@" << r.fps
                    << "  " << r.duration_seconds() << "s"
                    << "  lost=" << r.packets_lost << "\n";
        }
        session_recorder::shutdown();
        return 0;
      }

      int profiles_cli(int argc, char **argv) {
        app_profiler_cfg_t cfg;
        app_profiler::init(cfg);
        if (argc < 1) {
          auto list = app_profiler::list();
          std::cout << "profiles:\n";
          for (auto &p : list) {
            std::cout << "  " << p.name << "  match=\"" << p.match << "\""
                      << "  prio=" << p.priority << "  on=" << (p.enabled ? "y" : "n") << "\n";
          }
        } else if (std::strcmp(argv[0], "apply") == 0 && argc >= 2) {
          auto applied = app_profiler::apply_for_exec(argv[1]);
          std::cout << (applied ? "applied \"" + applied->name + "\"" : "no match") << "\n";
        }
        app_profiler::shutdown();
        return 0;
      }

      int presets_cli(int argc, char **argv) {
        if (argc < 1) {
          for (auto &p : codec_presets::all_presets())
            std::cout << "  " << p.name << "  (" << p.encoder << "/" << p.codec << ")\n";
          return 0;
        }
        auto *p = codec_presets::find(argv[0]);
        if (!p) { std::cout << "no such preset: " << argv[0] << "\n"; return 1; }
        std::cout << p->name << " (" << p->encoder << "/" << p->codec << ")\n";
        std::cout << "  " << p->description << "\n";
        for (auto &[k, v] : p->kv) std::cout << "  " << k << " = " << v << "\n";
        return 0;
      }

      int network_cli() {
        auto snap = network_probe::snapshot();
        std::cout << "aggregate score: " << snap.aggregate_score << "\n";
        for (auto &t : snap.targets) {
          std::cout << "  " << t.name << "  rtt_p95=" << t.rtt_p95_ms << "ms"
                    << "  jitter=" << t.jitter_ms << "ms"
                    << "  loss=" << t.loss_pct << "%"
                    << "  score=" << t.score << "\n";
        }
        return 0;
      }

      void help() {
        std::cout <<
          "solarctl -- SolarFlare command-line interface\n"
          "\n"
          "Usage: sunshine solarctl <command> [args]\n"
          "\n"
          "Commands:\n"
          "  status                 running state summary\n"
          "  telemetry              one-shot telemetry snapshot\n"
          "  health                 current health snapshot\n"
          "  sessions [N]           list recent sessions (default 10)\n"
          "  profiles [apply <exec>] list profiles or apply one for an exec\n"
          "  presets [name]         list codec presets or show one\n"
          "  network                network quality snapshot\n"
          "  help                   this help\n";
      }
    }  // namespace

    void print_help(const char *name) { (void)name; help(); }

    int run(const char *name, int argc, char **argv) {
      if (argc < 1) { help(); return 0; }
      std::string cmd = argv[0];
      int subc = argc - 1;
      char **subv = argv + 1;
      if (cmd == "status") return status();
      if (cmd == "telemetry") return telemetry_cli();
      if (cmd == "health") return health_cli();
      if (cmd == "sessions") return sessions_cli(subc, subv);
      if (cmd == "profiles") return profiles_cli(subc, subv);
      if (cmd == "presets") return presets_cli(subc, subv);
      if (cmd == "network") return network_cli();
      if (cmd == "help" || cmd == "--help" || cmd == "-h") { help(); return 0; }
      std::cout << "solarctl: unknown command '" << cmd << "'\n";
      help();
      return 2;
    }
  }  // namespace cli
}  // namespace solarflare
