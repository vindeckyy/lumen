/**
 * @file tests/unit/test_solarflare_modules.cpp
 * @brief One runnable check per SolarFlare subsystem.
 *
 * ponytail: non-trivial logic gets one assertion that fails if the
 * logic breaks. No fixtures, no parameterised suites, no edge-case
 * matrix -- those are YAGNI until something regresses.
 */
#include "../tests_common.h"

// local includes
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/network_probe.h"
#include "solarflare/runtime.h"
#include "solarflare/session_recorder.h"
#include "solarflare/telemetry.h"

namespace {

  // -------------------------------------------------------------------
  // Telemetry: round-trip a sample through the snapshot path.
  // -------------------------------------------------------------------
  TEST(TelemetryRoundTrip, SampleMakesItToSnapshot) {
    ASSERT_TRUE(solarflare::telemetry::init());
    solarflare::telemetry::record_latency(solarflare::STAGE_ENCODE, 1234);
    solarflare::telemetry::record_frame();
    auto s = solarflare::telemetry::snapshot();
    EXPECT_EQ(s.frames, 1u);
    EXPECT_EQ(s.latency[solarflare::STAGE_ENCODE].min_us, 1234u);
    EXPECT_EQ(s.latency[solarflare::STAGE_ENCODE].max_us, 1234u);
    solarflare::telemetry::shutdown();
  }

  // -------------------------------------------------------------------
  // Network probe: score formula boundary at the formula's design
  // point (rtt=30, jitter=5, loss=0 should give a perfect score).
  // -------------------------------------------------------------------
  TEST(NetworkProbeScore, PerfectNetworkIs100) {
    EXPECT_EQ(solarflare::network_probe::compute_score(30.0, 5.0, 0.0), 100.0);
  }

  TEST(NetworkProbeScore, HeavyLossDropsBelow40) {
    EXPECT_LT(solarflare::network_probe::compute_score(30.0, 5.0, 20.0), 40.0);
  }

  // -------------------------------------------------------------------
  // Adaptive bitrate: a no-op init/shutdown with default config.
  // ponytail: we don't simulate the policy here -- telemetry must be
  // up and that pulls in logging. Just check the lifecycle.
  // -------------------------------------------------------------------
  TEST(AdaptiveBitrateLifecycle, InitShutdownRoundTrip) {
    solarflare::adaptive_cfg_t c;
    c.enabled = false;  // don't actually subscribe; keeps the test fast
    EXPECT_TRUE(solarflare::adaptive_bitrate::init(c));
    EXPECT_FALSE(solarflare::adaptive_bitrate::snapshot().running == false);
    solarflare::adaptive_bitrate::shutdown();
  }

  // -------------------------------------------------------------------
  // Session recorder: a single record round-trips through disk.
  // -------------------------------------------------------------------
  TEST(SessionRecorderRoundTrip, WriteAndListOneRecord) {
    solarflare::session_cfg_t cfg;
    cfg.base_dir = std::filesystem::temp_directory_path() / "solarflare_test_sessions";
    std::filesystem::remove_all(cfg.base_dir);
    ASSERT_TRUE(solarflare::session_recorder::init(cfg));

    solarflare::session_info_t info;
    info.client_name = "tester";
    info.client_app = "smoketest";
    info.fps = 60;
    info.width = 1920;
    info.height = 1080;
    info.bitrate_kbps = 20000;
    auto id = solarflare::session_recorder::begin_session(info);
    EXPECT_FALSE(id.empty());

    solarflare::snapshot_t snap;
    snap.frames = 100;
    snap.bytes_sent = 1024;
    snap.packets_lost = 2;
    snap.fps_ewma = 60.0;
    snap.kbps_ewma = 5000.0;
    solarflare::session_recorder::end_session(snap);

    auto list = solarflare::session_recorder::list(10);
    ASSERT_EQ(list.size(), 1u);
    EXPECT_EQ(list[0].id, id);
    EXPECT_EQ(list[0].frames, 100u);
    EXPECT_EQ(list[0].client_name, "tester");
    EXPECT_TRUE(solarflare::session_recorder::remove(id));

    solarflare::session_recorder::shutdown();
    std::filesystem::remove_all(cfg.base_dir);
  }

  // -------------------------------------------------------------------
  // App profiler: substring match on the exec basename.
  // -------------------------------------------------------------------
  TEST(AppProfilerMatch, SubstringOnBasename) {
    solarflare::app_profiler_cfg_t cfg;
    cfg.profiles_path = std::filesystem::temp_directory_path() / "solarflare_test_profiles.json";
    std::filesystem::remove(cfg.profiles_path);
    ASSERT_TRUE(solarflare::app_profiler::init(cfg));

    solarflare::app_profile_t p;
    p.name = "Elden";
    p.match = "EldenRing";
    solarflare::app_profiler::upsert(p);

    auto m = solarflare::app_profiler::find_match("/games/EldenRing.exe");
    ASSERT_TRUE(m.has_value());
    EXPECT_EQ(m->name, "Elden");
    EXPECT_TRUE(solarflare::app_profiler::remove("Elden"));
    EXPECT_FALSE(solarflare::app_profiler::find_match("/games/EldenRing.exe").has_value());

    solarflare::app_profiler::shutdown();
    std::filesystem::remove(cfg.profiles_path);
  }

}  // namespace
