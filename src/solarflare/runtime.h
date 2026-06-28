/**
 * @file src/solarflare/runtime.h
 * @brief Lifecycle declarations for the SolarFlare subsystems.
 */
#pragma once

namespace solarflare {

  namespace runtime {
    /**
     * @brief Bring up every SolarFlare subsystem in the right order.
     * @return True on success.
     */
    bool init();

    /**
     * @brief Tear down every SolarFlare subsystem in reverse order.
     */
    void shutdown();
  }  // namespace runtime
}  // namespace solarflare
