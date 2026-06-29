/**
 * @file src/solarflare/solarflare.h
 * @brief Umbrella header for all SolarFlare subsystem modules.
 *
 * Including this header pulls in every SolarFlare C++ module that
 * the rest of the codebase may want to talk to. The umbrella is
 * intentionally header-only on the include side: the .cpp side of
 * each module compiles its own translation unit.
 *
 * The umbrella is intended for the *integration* files (main.cpp,
 * confighttp.cpp) -- not for callers that only need one module.
 * Single-module callers should include the specific header they need.
 */
#pragma once

#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/health_monitor.h"
#include "solarflare/network_heatmap.h"
#include "solarflare/network_probe.h"
#include "solarflare/predictive_abr.h"
#include "solarflare/session_recorder.h"
#include "solarflare/stutter_score.h"
#include "solarflare/telemetry.h"
