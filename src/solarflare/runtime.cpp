/**
 * @file src/solarflare/runtime.cpp
 * @brief Lifecycle glue for the SolarFlare subsystems.
 *
 * `runtime::init()` and `runtime::shutdown()` are called from
 * `main.cpp` to bring up / tear down all SolarFlare subsystems in
 * the right order. The order matters: telemetry must be up first
 * (everyone subscribes to it); the consumer subsystems (adaptive
 * bitrate, health monitor, live_data) come next; the persistent
 * subsystems (session recorder, app profiler) come last.
 *
 * The order on shutdown is the reverse of the order on init.
 */

#include "config.h"
#include "logging.h"
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/health_monitor.h"
#include "solarflare/inspector.h"
#include "solarflare/network_probe.h"
#include "solarflare/performance.h"
#include "solarflare/platform/runtime_config.h"
#include "solarflare/runtime.h"
#include "solarflare/session_recorder.h"
#include "solarflare/telemetry.h"

namespace solarflare {

  namespace runtime {

    bool init() {
      if (!telemetry::init()) return false;
      adaptive_bitrate::init(platform::make_adaptive_bitrate_config());
      health_monitor::init(platform::make_health_monitor_config());
      network_probe::init();
      session_recorder::init(platform::make_session_recorder_config());
      app_profiler::init(platform::make_app_profiler_config());
      inspector::init();
      performance::init(perf_cfg_t {});
      BOOST_LOG(info) << "SolarFlare runtime: up";
      return true;
    }

    void shutdown() {
      performance::shutdown();
      app_profiler::shutdown();
      session_recorder::shutdown();
      network_probe::shutdown();
      health_monitor::shutdown();
      adaptive_bitrate::shutdown();
      telemetry::shutdown();
    }
  }  // namespace runtime
}  // namespace solarflare
