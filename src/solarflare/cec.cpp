/**
 * @file src/solarflare/cec.cpp
 * @brief Definitions for the CEC ALLM controller.
 *
 * ponytail: popen("cec-client ... | cec-client ...") and parse the
 * one line we care about. Don't try to talk to the CEC adapter
 * ourselves -- cec-client is the proven abstraction.
 */

#ifdef __linux__
  #include <sys/wait.h>
#endif

// standard includes
#include <cstdio>
#include <cstdlib>
#include <sstream>

// local includes
#include "logging.h"
#include "solarflare/cec.h"

namespace solarflare {

  namespace allm {

    namespace {
      cec_snap_t g_snap {};

#ifdef __linux__
      // Returns the last line of stdout. Empty on failure.
      std::string run_cec(const std::string &args) {
        // Quick check that cec-client is on PATH.
        if (std::system(("command -v cec-client >/dev/null 2>&1")) != 0) {
          g_snap.state = allm_state_e::unsupported;
          g_snap.detail = "cec-client not on PATH";
          return {};
        }
        std::string cmd = "cec-client " + args + " 2>&1";
        FILE *fp = ::popen(cmd.c_str(), "r");
        if (!fp) {
          g_snap.state = allm_state_e::failed;
          g_snap.detail = "popen failed";
          return {};
        }
        std::string out;
        char buf[256];
        while (std::fgets(buf, sizeof(buf), fp)) out += buf;
        int rc = ::pclose(fp);
        if (rc != 0) {
          g_snap.state = allm_state_e::failed;
          g_snap.detail = "cec-client exit " + std::to_string(WEXITSTATUS(rc));
        }
        return out;
      }
#endif

      bool cec_supports_allm() {
#ifdef __linux__
        // Cheap probe: cec-client -s help 2>&1 should mention allm.
        std::string out = run_cec("-s help");
        return out.find("allm") != std::string::npos;
#else
        return false;
#endif
      }
    }  // namespace

    bool enable() {
#ifdef __linux__
      if (!cec_supports_allm()) {
        g_snap.state = allm_state_e::unsupported;
        g_snap.detail = "cec-client doesn't advertise ALLM";
        BOOST_LOG(warning) << "SolarFlare CEC: " << g_snap.detail;
        return false;
      }
      std::string out = run_cec("-s allm:on");
      if (g_snap.state == allm_state_e::failed) {
        BOOST_LOG(warning) << "SolarFlare CEC: " << g_snap.detail;
        return false;
      }
      g_snap.state = allm_state_e::on;
      g_snap.detail = "cec-client -s allm:on -> " + out.substr(0, out.find('\n'));
      BOOST_LOG(info) << "SolarFlare CEC: " << g_snap.detail;
      return true;
#else
      g_snap.state = allm_state_e::unsupported;
      g_snap.detail = "CEC not implemented on this platform";
      return false;
#endif
    }

    bool disable() {
#ifdef __linux__
      if (!cec_supports_allm()) return false;
      std::string out = run_cec("-s allm:off");
      if (g_snap.state == allm_state_e::failed) return false;
      g_snap.state = allm_state_e::off;
      g_snap.detail = "cec-client -s allm:off -> " + out.substr(0, out.find('\n'));
      BOOST_LOG(info) << "SolarFlare CEC: " << g_snap.detail;
      return true;
#else
      g_snap.state = allm_state_e::unsupported;
      return false;
#endif
    }

    cec_snap_t snapshot() { return g_snap; }
  }  // namespace allm
}  // namespace solarflare
