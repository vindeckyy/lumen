/**
 * @file src/solarflare/health_monitor.cpp
 * @brief Definitions for the SolarFlare health monitor.
 *
 * ponytail: one worker thread that runs every `interval` seconds,
 * takes 4 samples, fires alerts on threshold crosses. No per-resource
 * cooldown logic beyond a hash-of-(resource,severity).
 */

// standard includes
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <deque>
#include <fstream>
#include <limits>
#include <map>
#include <mutex>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
  #include <windows.h>
  #include <psapi.h>
#elif defined(__APPLE__)
  #include <mach/mach.h>
  #include <sys/statvfs.h>
  #include <sys/sysctl.h>
#else
  #include <sys/statvfs.h>
  #include <unistd.h>
#endif

#ifdef __linux__
  #include <filesystem>
#endif

// local includes
#include "logging.h"
#include "platform/common.h"
#include "solarflare/health_monitor.h"

namespace solarflare {

  namespace health_monitor {
    namespace {
      std::atomic<bool> g_running {false};
      health_cfg_t g_cfg {};
      std::mutex g_state_mtx;
      std::vector<resource_sample_t> g_latest;
      std::deque<alert_t> g_recent;  // newest first, capped 50
      std::mutex g_obs_mtx;
      alert_cb_t g_observer;
      std::map<std::string, std::chrono::steady_clock::time_point> g_last_alert;
      std::thread g_worker;
    }  // namespace

    // ponytail: each platform sampler is one syscall or two.
    double sample_cpu_pct() {
#ifdef _WIN32
      static FILETIME prev_idle {}, prev_kernel {}, prev_user {};
      FILETIME idle, kernel, user;
      if (!GetSystemTimes(&idle, &kernel, &user)) return 0.0;
      auto sub = [](FILETIME a, FILETIME b) {
        ULARGE_INTEGER x {.LowPart = a.dwLowDateTime, .HighPart = a.dwHighDateTime};
        ULARGE_INTEGER y {.LowPart = b.dwLowDateTime, .HighPart = b.dwHighDateTime};
        return static_cast<double>(x.QuadPart - y.QuadPart);
      };
      auto total = sub(kernel, prev_kernel) + sub(user, prev_user);
      auto idle_d = sub(idle, prev_idle);
      prev_idle = idle; prev_kernel = kernel; prev_user = user;
      return total > 0 ? 100.0 * (total - idle_d) / total : 0.0;
#elif defined(__APPLE__)
      // ponytail: mach_thread_basic_info over the task's threads.
      thread_array_t thrs; mach_msg_type_number_t cnt = 0;
      if (task_threads(mach_task_self(), &thrs, &cnt) != KERN_SUCCESS) return 0.0;
      double total = 0;
      for (mach_msg_type_number_t i = 0; i < cnt; ++i) {
        thread_basic_info_data_t b {}; mach_msg_type_number_t bc = THREAD_BASIC_INFO_COUNT;
        if (thread_info(thrs[i], THREAD_BASIC_INFO, (thread_info_t) &b, &bc) == KERN_SUCCESS && (b.flags & TH_FLAGS_IDLE) == 0)
          total += b.cpu_usage / 100.0;
        mach_port_deallocate(mach_task_self(), thrs[i]);
      }
      vm_deallocate(mach_task_self(), (vm_address_t) thrs, cnt * sizeof(*thrs));
      return total;
#else
      static std::uint64_t prev_total = 0, prev_idle = 0;
      std::ifstream f("/proc/stat");
      if (!f) return 0.0;
      std::string cpu; std::uint64_t u, ni, s, id;
      f >> cpu >> u >> ni >> s >> id;
      if (!f) return 0.0;
      std::uint64_t total = u + ni + s + id;
      if (prev_total == 0) { prev_total = total; prev_idle = id; return 0.0; }
      std::uint64_t dt = total - prev_total, di = id - prev_idle;
      prev_total = total; prev_idle = id;
      return dt > 0 ? 100.0 * (dt - di) / dt : 0.0;
#endif
    }

    double sample_mem_mib() {
#ifdef _WIN32
      PROCESS_MEMORY_COUNTERS pmc {};
      return GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof(pmc)) ? pmc.WorkingSetSize / (1024.0 * 1024.0) : 0.0;
#elif defined(__APPLE__)
      mach_task_basic_info_data_t b {}; mach_msg_type_number_t c = MACH_TASK_BASIC_INFO_COUNT;
      return task_info(mach_task_self(), MACH_TASK_BASIC_INFO, (task_info_t) &b, &c) == KERN_SUCCESS ? b.resident_size / (1024.0 * 1024.0) : 0.0;
#else
      std::ifstream f("/proc/self/status");
      if (!f) return 0.0;
      std::string line;
      while (std::getline(f, line)) {
        if (line.rfind("VmRSS:", 0) == 0) {
          double v; std::string unit;
          std::istringstream(line.substr(6)) >> v >> unit;
          return unit == "kB" ? v / 1024.0 : v;  // ponytail: assume kB on Linux.
        }
      }
      return 0.0;
#endif
    }

    double sample_thermal_c() {
#ifdef __linux__
      // ponytail: walk /sys/class/thermal, return max.
      double max_c = std::numeric_limits<double>::quiet_NaN();
      std::error_code ec;
      for (auto it = std::filesystem::directory_iterator("/sys/class/thermal", ec);
           !ec && it != std::filesystem::end(it, ec); it.increment(ec)) {
        std::ifstream f(it->path() / "temp");
        long raw = 0; f >> raw;
        if (f && raw > 0) {
          double c = raw / 1000.0;
          if (std::isnan(max_c) || c > max_c) max_c = c;
        }
      }
      return max_c;
#else
      return std::numeric_limits<double>::quiet_NaN();
#endif
    }

    double sample_disk_free_mib() {
#ifdef _WIN32
      DWORD sc, bs, fc, tc;
      if (!GetDiskFreeSpaceA(nullptr, &sc, &bs, &fc, &tc)) return 0.0;
      return static_cast<double>(fc) * sc * bs / (1024.0 * 1024.0);
#else
      struct statvfs s {};
      return statvfs(".", &s) == 0 ? static_cast<double>(s.f_bavail) * s.f_frsize / (1024.0 * 1024.0) : 0.0;
#endif
    }

    double sample_disk_total_mib() {
#ifdef _WIN32
      DWORD sc, bs, fc, tc;
      if (!GetDiskFreeSpaceA(nullptr, &sc, &bs, &fc, &tc)) return 0.0;
      return static_cast<double>(tc) * sc * bs / (1024.0 * 1024.0);
#else
      struct statvfs s {};
      return statvfs(".", &s) == 0 ? static_cast<double>(s.f_blocks) * s.f_frsize / (1024.0 * 1024.0) : 0.0;
#endif
    }

    namespace {
      // ponytail: raise an alert; dedup on (resource, severity) per cooldown.
      void raise(alert_severity sev, const std::string &res, double v, double thr, const std::string &msg) {
        std::string key = res + ":" + std::to_string(static_cast<int>(sev));
        auto now = std::chrono::steady_clock::now();
        auto it = g_last_alert.find(key);
        if (it != g_last_alert.end() && now - it->second < g_cfg.alert_cooldown) return;
        g_last_alert[key] = now;
        alert_t a { .at = std::chrono::system_clock::now(), .severity = sev, .resource = res, .message = msg, .value = v, .threshold = thr };
        {
          std::lock_guard<std::mutex> lock(g_state_mtx);
          g_recent.push_front(a);
          while (g_recent.size() > 50) g_recent.pop_back();
        }
        switch (sev) {
          case alert_severity::info:     BOOST_LOG(info)    << "[SolarFlare health] " << res << ": " << msg; break;
          case alert_severity::warning:  BOOST_LOG(warning) << "[SolarFlare health] " << res << ": " << msg; break;
          case alert_severity::critical: BOOST_LOG(error)   << "[SolarFlare health] " << res << ": " << msg; break;
        }
        alert_cb_t obs;
        { std::lock_guard<std::mutex> l(g_obs_mtx); obs = g_observer; }
        if (obs) try { obs(a); } catch (...) {}
      }

      void worker_loop() {
        platf::set_thread_name("solarflare-health");
        while (g_running.load(std::memory_order_acquire)) {
          std::vector<resource_sample_t> samples;
          double cpu = sample_cpu_pct();
          double mem = sample_mem_mib();
          double thermal = sample_thermal_c();
          double disk_free = sample_disk_free_mib();
          double disk_total = sample_disk_total_mib();
          double disk_pct = disk_total > 0 ? 100.0 * disk_free / disk_total : 100.0;

          samples.push_back({"cpu", cpu});
          samples.push_back({"memory_mib", mem});
          if (!std::isnan(thermal)) samples.push_back({"thermal_c", thermal});
          samples.push_back({"disk_free_pct", disk_pct});

          // ponytail: alerts in one place; thresholds in config.
          if (cpu >= g_cfg.cpu_crit_pct) raise(alert_severity::critical, "cpu", cpu, g_cfg.cpu_crit_pct, "cpu >= crit");
          else if (cpu >= g_cfg.cpu_warn_pct) raise(alert_severity::warning, "cpu", cpu, g_cfg.cpu_warn_pct, "cpu >= warn");
          if (disk_pct <= g_cfg.disk_crit_free_pct) raise(alert_severity::critical, "disk", disk_pct, g_cfg.disk_crit_free_pct, "disk free <= crit");
          else if (disk_pct <= g_cfg.disk_warn_free_pct) raise(alert_severity::warning, "disk", disk_pct, g_cfg.disk_warn_free_pct, "disk free <= warn");
          if (!std::isnan(thermal)) {
            if (thermal >= g_cfg.thermal_crit_c) raise(alert_severity::critical, "thermal", thermal, g_cfg.thermal_crit_c, "thermal >= crit");
            else if (thermal >= g_cfg.thermal_warn_c) raise(alert_severity::warning, "thermal", thermal, g_cfg.thermal_warn_c, "thermal >= warn");
          }
          {
            std::lock_guard<std::mutex> lock(g_state_mtx);
            g_latest = std::move(samples);
          }
          std::this_thread::sleep_for(g_cfg.interval);
        }
      }
    }  // namespace

    bool init(const health_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      g_worker = std::thread(worker_loop);
      BOOST_LOG(info) << "SolarFlare health: up interval=" << cfg.interval.count() << "s";
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      if (g_worker.joinable()) g_worker.join();
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = nullptr;
    }

    void apply_config(const health_cfg_t &cfg) { g_cfg = cfg; }

    health_snap_t snapshot() {
      health_snap_t out;
      out.captured_at = std::chrono::system_clock::now();
      std::lock_guard<std::mutex> lock(g_state_mtx);
      out.resources = g_latest;
      out.recent_alerts.assign(g_recent.begin(), g_recent.end());
      return out;
    }

    void set_observer(alert_cb_t cb) {
      std::lock_guard<std::mutex> lock(g_obs_mtx);
      g_observer = std::move(cb);
    }
  }  // namespace health_monitor
}  // namespace solarflare
