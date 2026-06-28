/**
 * @file src/solarflare/cli.h
 * @brief SolarFlare CLI (`solarctl`): thin wrapper over the C++ modules.
 *
 * `solarctl` runs as a subcommand of the `sunshine` binary
 * (`sunshine solarctl <verb> [args]`). It exists so a user can
 * inspect state, apply a codec preset, list sessions, etc. without
 * touching the web UI.
 */
#pragma once

namespace solarflare {

  namespace cli {
    /**
     * @brief Entry point for `sunshine solarctl ...`.
     * @param name Program name (argv[0]).
     * @param argc Argument count (does NOT include "solarctl" itself).
     * @param argv Argument vector.
     * @return Exit code (0 = success).
     */
    int run(const char *name, int argc, char **argv);

    /**
     * @brief Print help to stdout.
     */
    void print_help(const char *name);
  }  // namespace cli
}  // namespace solarflare
