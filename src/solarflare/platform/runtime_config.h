/**
 * @file src/solarflare/platform/runtime_config.h
 * @brief SolarFlare runtime config factories.
 *
 * ponytail: thin helpers that materialise the per-subsystem configs.
 * If a single field ever changes, edit it here, not in three modules.
 */
#pragma once

// local includes
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/health_monitor.h"
#include "solarflare/session_recorder.h"

namespace solarflare { namespace platform {
  adaptive_cfg_t make_adaptive_bitrate_config();
  health_cfg_t make_health_monitor_config();
  session_cfg_t make_session_recorder_config();
  app_profiler_cfg_t make_app_profiler_config();
}}