/**
 * @file src/solarflare/health_monitor.h
 * @brief SolarFlare health monitor: periodic CPU/mem/disk sampling.
 *
 * ponytail: one worker thread, four platform samplers, one alert
 * queue. We don't take autonomous recovery actions -- the adaptive
 * bitrate controller watches telemetry for that.
 */
#pragma once

// standard includes
#include <chrono>
#include <cstdint>
#include <functional>
#include <string>
#include <vector>

namespace solarflare {

  enum class alert_severity { info, warning, critical };

  struct alert_t {
    std::chrono::system_clock::time_point at;
    alert_severity severity = alert_severity::info;
    std::string resource;
    std::string message;
    double value = 0.0, threshold = 0.0;
  };

  struct resource_sample_t {
    std::string name;
    double value = 0.0;
  };

  struct health_snap_t {
    std::vector<resource_sample_t> resources;
    std::vector<alert_t> recent_alerts;  // newest first, capped at 50
    std::chrono::system_clock::time_point captured_at;
  };

  struct health_cfg_t {
    std::chrono::seconds interval {2};
    std::chrono::seconds alert_cooldown {30};
    double cpu_warn_pct = 85.0, cpu_crit_pct = 95.0;
    double mem_warn_pct = 85.0, mem_crit_pct = 95.0;
    double disk_warn_free_pct = 10.0, disk_crit_free_pct = 5.0;
    double thermal_warn_c = 80.0, thermal_crit_c = 90.0;
  };

  namespace health_monitor {
    bool init(const health_cfg_t &cfg);
    void shutdown();
    void apply_config(const health_cfg_t &cfg);
    health_snap_t snapshot();

    // ponytail: one observer slot. The web UI binds this when the
    // dashboard wants live alerts.
    using alert_cb_t = std::function<void(const alert_t &)>;
    void set_observer(alert_cb_t cb);

    // ponytail: platform samplers are simple; default impls work on every OS.
    double sample_cpu_pct();
    double sample_mem_mib();
    double sample_thermal_c();  // NaN if unavailable
    double sample_disk_free_mib();
    double sample_disk_total_mib();
  }  // namespace health_monitor
}  // namespace solarflare
