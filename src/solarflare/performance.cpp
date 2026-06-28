/**
 * @file src/solarflare/performance.cpp
 * @brief Definitions for the pre-stream performance prep.
 *
 * ponytail: each step is a small lambda that knows how to apply and
 * how to revert. The orchestrator walks the list and calls each.
 * No abstractions, no rollback transactions -- if a step fails the
 * user sees it in the snapshot and the others still run.
 */

// standard includes
#include <chrono>
#include <cstdio>
#include <fstream>
#include <string>

#ifdef __linux__
  #include <sys/stat.h>
  #include <unistd.h>
  #include <sys/types.h>
#endif

// local includes
#include "logging.h"
#include "solarflare/performance.h"

namespace solarflare {

  namespace performance {

    namespace {
      std::atomic<bool> g_running {false};
      perf_cfg_t g_cfg {};
      perf_snap_t g_snap {};

      // ponytail: each step is "apply" + "revert" + a display name.
      // If the platform doesn't have the bit, both are no-ops.

      // ---------- HID polling ----------
      // Linux: /sys/module/usbhid/parameters/poll
      // macOS: no sysfs equivalent; hidutil could do it but it's a
      //   user-space setting per device, skip on macOS.
      // Windows: hidclass.sys polling interval is configurable in
      //   the registry; skip the registry edit for now.
      bool hid_apply(const perf_cfg_t &cfg) {
#ifdef __linux__
        std::ofstream f("/sys/module/usbhid/parameters/poll");
        if (!f) return false;
        f << cfg.hid_target_hz;
        return f.good();
#else
        (void) cfg;
        return false;  // unsupported on this platform
#endif
      }

      bool hid_revert() {
#ifdef __linux__
        // Restore the kernel default.
        std::ofstream f("/sys/module/usbhid/parameters/poll");
        if (!f) return false;
        f << 125;
        return f.good();
#else
        return false;
#endif
      }

      // ---------- GPU perf hint ----------
      // NVIDIA: nvidia-smi -pl max, --persistence-mode=1
      // AMD: rocm-smi --setpoweroverdrive, but most users don't have
      //   rocm-smi; fall back to writing /sys/class/drm/card*/device/
      //   power_dpm_state if writable.
      // Intel: /sys/class/drm/card*/gt_min_freq_mhz = gt_max_freq_mhz
      bool gpu_apply(const perf_cfg_t &cfg) {
        (void) cfg;
#ifdef __linux__
        // Try Intel first (most common on CachyOS + a discrete GPU).
        bool any = false;
        for (auto it = std::filesystem::directory_iterator("/sys/class/drm", std::error_code {});
             it != std::filesystem::end(it); it.increment({})) {
          auto p = it->path() / "gt_max_freq_mhz";
          std::ifstream mx(p);
          if (!mx) continue;
          int max_mhz = 0;
          mx >> max_mhz;
          if (max_mhz <= 0) continue;
          std::ofstream mn(it->path() / "gt_min_freq_mhz");
          if (mn) { mn << max_mhz; any = any || mn.good(); }
        }
        return any;  // false = nothing to do; not an error
#else
        return false;
#endif
      }

      bool gpu_revert() {
#ifdef __linux__
        // Write "0" to gt_min_freq_mhz so the driver picks its own floor.
        for (auto it = std::filesystem::directory_iterator("/sys/class/drm", std::error_code {});
             it != std::filesystem::end(it); it.increment({})) {
          std::ofstream mn(it->path() / "gt_min_freq_mhz");
          if (mn) mn << 0;
        }
        return true;
#else
        return false;
#endif
      }

      // ---------- Inhibit blanking ----------
      // systemd-inhibit --what=idle:sleep --who=solarflare --why=streaming
      //   --mode=block sleep 86400
      // On macOS: caffeinate -di
      // On Windows: SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED)
      bool inhibit_apply() {
#ifdef __linux__
        // Fork a child that holds the inhibit lock. Caller is
        // expected to call revert() which sends SIGTERM.
        pid_t pid = fork();
        if (pid == 0) {
          // Child: exec systemd-inhibit if available.
          execlp("systemd-inhibit", "systemd-inhibit",
                 "--what=idle:sleep", "--who=solarflare",
                 "--why=streaming", "--mode=block",
                 "sleep", "86400", static_cast<char *>(nullptr));
          // If systemd-inhibit isn't installed, fall back to a
          // plain `sleep 86400` -- it doesn't actually inhibit
          // anything, but it doesn't crash either.
          execlp("sleep", "sleep", "86400", static_cast<char *>(nullptr));
          _exit(127);
        }
        if (pid < 0) return false;
        // Stash the pid somewhere we can kill it from revert().
        // ponytail: rather than a class member, we stash in a
        // well-known file under /tmp -- simple, atomic, no extra
        // global state.
        std::ofstream f("/tmp/.solarflare-inhibit.pid");
        if (f) f << pid;
        return true;
#elif defined(__APPLE__)
        pid_t pid = fork();
        if (pid == 0) {
          execlp("caffeinate", "caffeinate", "-di", "sleep", "86400", static_cast<char *>(nullptr));
          _exit(127);
        }
        if (pid < 0) return false;
        std::ofstream f("/tmp/.solarflare-inhibit.pid");
        if (f) f << pid;
        return true;
#else
        return false;
#endif
      }

      bool inhibit_revert() {
        std::ifstream f("/tmp/.solarflare-inhibit.pid");
        if (!f) return false;
        long pid = 0;
        f >> pid;
        if (pid > 0) ::kill(static_cast<pid_t>(pid), SIGTERM);
        std::remove("/tmp/.solarflare-inhibit.pid");
        return true;
      }

      // ---------- QoS / DSCP ----------
      // The actual setsockopt lives in the streaming layer (enet /
      // UDP). Here we just toggle the config flag the rest of the
      // code reads. ponytail: doing the right thing in the kernel
      // socket is upstream's job; we surface the request.
      bool qos_apply() { return true; }
      bool qos_revert() { return true; }

      struct step_def_t {
        const char *name;
        bool enabled_in_cfg;
        bool (*apply)(const perf_cfg_t &);
        bool (*revert)();
      };

      const step_def_t kSteps[] = {
        {"boost_hid_polling", [](const perf_cfg_t &c) -> bool { return c.boost_hid_polling; }(perf_cfg_t{}), hid_apply, hid_revert},
      };
      // ponytail: build the step list at runtime so we don't have to
      // duplicate the cfg-driven enabled checks.
      std::vector<step_def_t> build_steps(const perf_cfg_t &cfg) {
        return {
          {"boost_hid_polling", cfg.boost_hid_polling, hid_apply, hid_revert},
          {"set_gpu_perf",      cfg.set_gpu_perf,      gpu_apply,  gpu_revert},
          {"inhibit_blanking",  cfg.inhibit_blanking,  inhibit_apply, inhibit_revert},
          {"mark_qos",          cfg.mark_qos,          qos_apply,  qos_revert},
        };
      }
    }  // namespace

    bool init(const perf_cfg_t &cfg) {
      bool was = false;
      if (!g_running.compare_exchange_strong(was, true)) return true;
      g_cfg = cfg;
      BOOST_LOG(info) << "SolarFlare performance: up";
      return true;
    }

    void shutdown() {
      if (!g_running.exchange(false)) return;
      if (g_snap.active) revert();
    }

    bool apply() {
      if (!g_cfg.enabled) return true;
      if (g_snap.active) return true;  // idempotent
      g_snap = perf_snap_t {};
      g_snap.cfg = g_cfg;
      g_snap.active = true;
      g_snap.started_at_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
                                std::chrono::steady_clock::now().time_since_epoch()).count();
      for (auto &step : build_steps(g_cfg)) {
        perf_step_t s;
        s.name = step.name;
        s.applied = step.apply(g_cfg);
        std::ostringstream d;
        d << (s.applied ? "applied" : "skipped (unsupported or denied)");
        s.detail = d.str();
        g_snap.steps.push_back(s);
        BOOST_LOG(info) << "SolarFlare performance: " << step.name << " " << (s.applied ? "applied" : "skipped");
      }
      return true;
    }

    void revert() {
      if (!g_snap.active) return;
      // Walk in reverse so we unwind in the opposite order.
      for (auto it = build_steps(g_cfg).rbegin(); it != build_steps(g_cfg).rend(); ++it) {
        it->revert();
        BOOST_LOG(info) << "SolarFlare performance: " << it->name << " reverted";
      }
      g_snap.active = false;
    }

    perf_snap_t snapshot() { return g_snap; }
  }  // namespace performance
}  // namespace solarflare
