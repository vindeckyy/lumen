/**
 * @file tests/unit/test_solarflare_modules.cpp
 * @brief One runnable check per SolarFlare subsystem.
 *
 * ponytail: non-trivial logic gets one assertion that fails if the
 * logic breaks. No fixtures, no per-edge-case matrix -- those are
 * YAGNI until something regresses.
 */
#include "../tests_common.h"

// local includes
#include "solarflare/adaptive_bitrate.h"
#include "solarflare/app_profiler.h"
#include "solarflare/codec_presets.h"
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
  // Telemetry observer: fires synchronously on record_latency.
  // -------------------------------------------------------------------
  TEST(TelemetryObserver, FiresOnRecord) {
    ASSERT_TRUE(solarflare::telemetry::init());
    int calls = 0;
    solarflare::telemetry::set_observer([&](const solarflare::snapshot_t &) { ++calls; });
    solarflare::telemetry::record_latency(solarflare::STAGE_SEND, 100);
    EXPECT_EQ(calls, 1);
    solarflare::telemetry::set_observer(nullptr);
    solarflare::telemetry::record_latency(solarflare::STAGE_SEND, 100);
    EXPECT_EQ(calls, 1);  // observer cleared, no extra call
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
    c.enabled = false;  // skip the telemetry observer install
    EXPECT_TRUE(solarflare::adaptive_bitrate::init(c));
    EXPECT_TRUE(solarflare::adaptive_bitrate::snapshot().running);
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

  // -------------------------------------------------------------------
  // Codec presets: every preset has the required fields, encoder is
  // one of the known set.
  // -------------------------------------------------------------------
  TEST(CodecPresets, AllHaveNameEncoderCodec) {
    auto presets = solarflare::codec_presets::all_presets();
    ASSERT_FALSE(presets.empty());
    for (auto &p : presets) {
      EXPECT_FALSE(p.name.empty()) << "preset missing name";
      EXPECT_FALSE(p.encoder.empty()) << p.name << " missing encoder";
      EXPECT_FALSE(p.codec.empty()) << p.name << " missing codec";
      EXPECT_FALSE(p.kv.empty()) << p.name << " has no settings";
    }
  }

  TEST(CodecPresets, FindByName) {
    auto *p = solarflare::codec_presets::find("NVENC Low Latency 1080p60");
    ASSERT_NE(p, nullptr);
    EXPECT_EQ(p->encoder, "nvenc");
    EXPECT_EQ(p->codec, "h264");
  }

  TEST(CodecPresets, FiltersByEncoder) {
    auto nvenc = solarflare::codec_presets::presets_for_encoder("nvenc");
    EXPECT_GE(nvenc.size(), 4u);
    for (auto &p : nvenc) EXPECT_EQ(p.encoder, "nvenc");
  }

  TEST(CodecPresets, JsonShape) {
    auto j = solarflare::codec_presets::all_presets_json();
    ASSERT_TRUE(j.is_array());
    ASSERT_GE(j.size(), 1u);
    auto &first = j[0];
    EXPECT_TRUE(first.contains("name"));
    EXPECT_TRUE(first.contains("encoder"));
    EXPECT_TRUE(first.contains("codec"));
    EXPECT_TRUE(first.contains("kv"));
  }

  // -------------------------------------------------------------------
  // Health monitor: lifecycle + the no-platforms-available samplers
  // don't crash. ponytail: we don't test actual CPU numbers -- they
  // vary by host -- only that the calls are safe.
  // -------------------------------------------------------------------
  TEST(HealthMonitorLifecycle, SamplersAreSafe) {
    (void) solarflare::health_monitor::sample_cpu_pct();
    (void) solarflare::health_monitor::sample_mem_mib();
    (void) solarflare::health_monitor::sample_thermal_c();
    (void) solarflare::health_monitor::sample_disk_free_mib();
    (void) solarflare::health_monitor::sample_disk_total_mib();
    SUCCEED();
  }

  // -------------------------------------------------------------------
  // Inspector: probe returns a populated report + JSON shape.
  // -------------------------------------------------------------------
  TEST(InspectorLifecycle, ProbeReturnsReport) {
    auto r = solarflare::inspector::probe();
    // OS name should be non-empty on every supported platform.
    EXPECT_FALSE(r.os_name.empty());
    // Recommended codec should be one of the three.
    EXPECT_TRUE(r.recommended_codec == "h264" || r.recommended_codec == "hevc" || r.recommended_codec == "av1");
  }

  TEST(InspectorJson, Shape) {
    auto r = solarflare::inspector::probe();
    auto j = solarflare::inspector::to_json(r);
    EXPECT_TRUE(j.contains("os"));
    EXPECT_TRUE(j.contains("cpu"));
    EXPECT_TRUE(j.contains("gpu"));
    EXPECT_TRUE(j.contains("recommended"));
    EXPECT_TRUE(j.contains("warnings"));
  }

  // -------------------------------------------------------------------
  // Latency budget: from_snapshot produces a sane split.
  // -------------------------------------------------------------------
  TEST(LatencyBudget, FromSnapshot) {
    solarflare::snapshot_t s;
    s.latency[solarflare::STAGE_ENCODE].submit(2000);
    s.latency[solarflare::STAGE_SEND].submit(1000);
    s.latency[solarflare::STAGE_NETWORK_RTT].submit(20000);
    s.session.active = true;
    s.session.fps = 60;
    auto b = solarflare::latency_budget::from_snapshot(s, 4000, 8000);
    EXPECT_EQ(b.encode_us, 2000u);
    EXPECT_EQ(b.send_us, 1000u);
    EXPECT_EQ(b.network_rtt_us, 20000u);
    EXPECT_EQ(b.frame_budget_us, 16667u);  // 1_000_000 / 60
    EXPECT_TRUE(b.fits_in_budget());
    EXPECT_EQ(b.client_total_us(), 10000u + 4000u + 8000u);  // half rtt + decode + display
  }

  // -------------------------------------------------------------------
  // Performance prep: lifecycle doesn't crash; apply/revert idempotent.
  // -------------------------------------------------------------------
  TEST(PerformanceLifecycle, ApplyRevertIdempotent) {
    solarflare::perf_cfg_t c;
    solarflare::performance::init(c);
    EXPECT_TRUE(solarflare::performance::apply());
    EXPECT_TRUE(solarflare::performance::snapshot().active);
    EXPECT_TRUE(solarflare::performance::apply());  // second call = no-op
    solarflare::performance::revert();
    EXPECT_FALSE(solarflare::performance::snapshot().active);
    solarflare::performance::shutdown();
  }

  // -------------------------------------------------------------------
  // Predictive ABR: snapshot is populated after init.
  // ponytail: we don't drive the controller in a unit test -- the
  // policy reads from the telemetry observer which is shared global
  // state. Just check the snapshot shape + lifecycle.
  // -------------------------------------------------------------------
  TEST(PredictiveAbrLifecycle, InitAndSnapshot) {
    EXPECT_TRUE(solarflare::predictive_abr::init({}));
    auto s = solarflare::predictive_abr::snapshot();
    EXPECT_TRUE(s.running);
    solarflare::predictive_abr::shutdown();
  }

  // -------------------------------------------------------------------
  // Stutter score: a single large hitch drops the score.
  // -------------------------------------------------------------------
  TEST(StutterScore, HitchDropsScore) {
    solarflare::stutter_score::reset();
    // 30 normal 60-fps frames (16667 us each), then one 33 ms hitch.
    for (int i = 0; i < 30; ++i) solarflare::stutter_score::record(16667);
    solarflare::stutter_score::record(33000);  // 2x expected -> hitch
    auto s = solarflare::stutter_score::snapshot(60);
    EXPECT_LT(s.score, 100.0);
    EXPECT_GE(s.hitches, 1u);
  }

  TEST(StutterScore, PerfectStreamScores100) {
    solarflare::stutter_score::reset();
    for (int i = 0; i < 100; ++i) solarflare::stutter_score::record(16667);
    auto s = solarflare::stutter_score::snapshot(60);
    EXPECT_EQ(s.hitches, 0u);
    EXPECT_EQ(s.score, 100.0);
  }

  // -------------------------------------------------------------------
  // Network heatmap: a target accumulates into 60 buckets.
  // -------------------------------------------------------------------
  TEST(NetworkHeatmap, OneTargetSixtyBuckets) {
    solarflare::network_heatmap::init();
    solarflare::network_heatmap::register_target("t");
    for (int i = 0; i < 100; ++i) solarflare::network_heatmap::submit_rtt("t", 20 + i % 50);
    solarflare::network_heatmap::submit_loss("t", 1000, 5);
    auto h = solarflare::network_heatmap::snapshot();
    ASSERT_EQ(h.targets.size(), 1u);
    EXPECT_EQ(h.targets[0].buckets.size(), 60u);
    solarflare::network_heatmap::shutdown();
  }

  // -------------------------------------------------------------------
  // Runtime: init + shutdown all subsystems.
  // -------------------------------------------------------------------
  TEST(RuntimeLifecycle, FullCycle) {
    EXPECT_TRUE(solarflare::runtime::init());
    solarflare::runtime::shutdown();
    SUCCEED();
  }

}  // namespace
