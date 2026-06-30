/**
 * @file src/lumen_metrics.cpp
 * @brief Definitions for the Lumen metrics registry.
 */

// standard includes
#include <sstream>

// local includes
#include "lumen_metrics.h"

namespace lumen {

  std::atomic<int64_t> metric_up {1};
  std::atomic<int64_t> metric_active_streams {0};
  std::atomic<int64_t> metric_bytes_sent {0};
  std::atomic<int64_t> metric_frames_encoded {0};
  std::atomic<int64_t> metric_frames_dropped {0};
  std::atomic<int64_t> metric_http_requests {0};

  namespace {

    // One helper per metric keeps render_metrics() trivially auditable.
    void emit_gauge(std::ostringstream &out, const char *name, const char *help, int64_t v) {
      out << "# HELP lumen_" << name << ' ' << help << '\n'
          << "# TYPE lumen_" << name << " gauge\n"
          << "lumen_" << name << ' ' << v << '\n';
    }

    void emit_counter(std::ostringstream &out, const char *name, const char *help, int64_t v) {
      out << "# HELP lumen_" << name << ' ' << help << '\n'
          << "# TYPE lumen_" << name << " counter\n"
          << "lumen_" << name << ' ' << v << '\n';
    }

  }  // namespace

  std::string render_metrics() {
    std::ostringstream out;
    emit_gauge(out, "up", "Whether the streaming host is alive (1) or not (0).", metric_up.load());
    emit_gauge(out, "active_streams", "Number of streams currently active.", metric_active_streams.load());
    emit_counter(out, "bytes_sent", "Total bytes sent to clients since process start.", metric_bytes_sent.load());
    emit_counter(out, "frames_encoded", "Total frames encoded since process start.", metric_frames_encoded.load());
    emit_counter(out, "frames_dropped", "Total frames dropped since process start.", metric_frames_dropped.load());
    emit_counter(out, "http_requests", "Total HTTP requests served by the confighttp server.", metric_http_requests.load());
    return out.str();
  }

}  // namespace lumen