/**
 * @file src/solarflare/platform/runtime_config.cpp
 * @brief Definitions of the SolarFlare runtime config factories.
 */
#include "config.h"
#include "solarflare/platform/runtime_config.h"

namespace solarflare { namespace platform {
  // ponytail: project the user-facing config::solarflare onto the
  // per-subsystem structs. Defaults in config.h are the source of
  // truth, these factories are just bridges.
  adaptive_cfg_t make_adaptive_bitrate_config() {
    adaptive_cfg_t c;
    c.enabled = config::solarflare.adaptive_bitrate_enabled;
    c.floor_kbps = static_cast<std::uint32_t>(config::solarflare.adaptive_floor_kbps);
    c.ceiling_kbps = static_cast<std::uint32_t>(config::solarflare.adaptive_ceiling_kbps);
    c.step_kbps = static_cast<std::uint32_t>(config::solarflare.adaptive_step_kbps);
    c.cooldown = std::chrono::milliseconds(config::solarflare.adaptive_cooldown_ms);
    c.loss_threshold_pct = config::solarflare.adaptive_loss_threshold_pct;
    c.rtt_threshold_ms = config::solarflare.adaptive_rtt_threshold_ms;
    c.rtt_recover_ms = config::solarflare.adaptive_rtt_recover_ms;
    return c;
  }
  health_cfg_t make_health_monitor_config() {
    health_cfg_t c;
    c.interval = std::chrono::seconds(config::solarflare.health_sample_interval_s);
    c.alert_cooldown = std::chrono::seconds(config::solarflare.health_alert_cooldown_s);
    c.cpu_warn_pct = config::solarflare.health_cpu_warn_pct;
    c.cpu_crit_pct = config::solarflare.health_cpu_crit_pct;
    c.mem_warn_pct = config::solarflare.health_memory_warn_pct;
    c.mem_crit_pct = config::solarflare.health_memory_crit_pct;
    c.disk_warn_free_pct = config::solarflare.health_disk_warn_pct_free;
    c.disk_crit_free_pct = config::solarflare.health_disk_crit_pct_free;
    c.thermal_warn_c = config::solarflare.health_thermal_warn_c;
    c.thermal_crit_c = config::solarflare.health_thermal_crit_c;
    return c;
  }
  session_cfg_t make_session_recorder_config() {
    session_cfg_t c;
    c.enabled = config::solarflare.session_recorder_enabled;
    c.max_index_entries = static_cast<std::uint32_t>(config::solarflare.session_max_index_entries);
    return c;
  }
  app_profiler_cfg_t make_app_profiler_config() {
    app_profiler_cfg_t c;
    c.enabled = config::solarflare.app_profiler_enabled;
    return c;
  }
}}