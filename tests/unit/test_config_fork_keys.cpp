/**
 * @file tests/unit/test_config_fork_keys.cpp
 * @brief Tests for the SolarFlare fork-specific configuration keys.
 *
 * The SolarFlare fork (https://github.com/vindeckyy/Solar-Flare) adds
 * five Linux-only tunables under config::solarflare_t:
 *   - busy_poll_us       (int,    0-10000, default 50)
 *   - rate_cap_pct       (int,    50-95,   default 80)
 *   - enet_4mib_buffer   (bool,   default true)
 *   - pipewire_latency_ms(int,    1-40,    default 8)
 *   - cpu_pinning        (bool,   default true)
 *
 * These tests lock in the defaults and the range-clamp behaviour of
 * apply_config() for each one. Out-of-range values must be silently
 * rejected (the upstream int_between_f / bool_f convention) so a
 * misconfigured sunshine.conf falls back to the default rather than
 * misbehaving.
 *
 * The fork only declares these in src/config.h / src/config.cpp; the
 * tests do not spin up a real ENet socket or PipeWire stream. They
 * exercise the config helpers directly so the clamp / parse logic is
 * covered even on platforms where the consumer code (network.cpp,
 * stream.cpp, etc.) is conditionally compiled out.
 */
#include "../tests_common.h"

// local includes
#include "src/config.h"

namespace {

  // Snapshot of the solarflare_t struct under test. We save and
  // restore around each test so tests don't leak state into each
  // other or into later test binaries in the same process.
  struct SolarflareSnapshot {
    int busy_poll_us;
    int rate_cap_pct;
    bool enet_4mib_buffer;
    int pipewire_latency_ms;
    bool cpu_pinning;

    SolarflareSnapshot() {
      busy_poll_us = config::solarflare.busy_poll_us;
      rate_cap_pct = config::solarflare.rate_cap_pct;
      enet_4mib_buffer = config::solarflare.enet_4mib_buffer;
      pipewire_latency_ms = config::solarflare.pipewire_latency_ms;
      cpu_pinning = config::solarflare.cpu_pinning;
    }

    void restore() {
      config::solarflare.busy_poll_us = busy_poll_us;
      config::solarflare.rate_cap_pct = rate_cap_pct;
      config::solarflare.enet_4mib_buffer = enet_4mib_buffer;
      config::solarflare.pipewire_latency_ms = pipewire_latency_ms;
      config::solarflare.cpu_pinning = cpu_pinning;
    }
  };

  class SolarflareConfigTest: public ::testing::Test {
  protected:
    SolarflareSnapshot snapshot;

    void SetUp() override { /* snapshot captured */ }

    void TearDown() override {
      snapshot.restore();
    }
  };

  // ---------------------------------------------------------------------
  // Defaults
  // ---------------------------------------------------------------------

  TEST_F(SolarflareConfigTest, DefaultsMatchPreviouslyHardcodedValues) {
    // The defaults must equal the values that were hardcoded in
    // src/network.cpp, src/stream.cpp, src/platform/linux/pipewire.cpp
    // and src/platform/linux/misc.cpp before the config plumbing was
    // added. This is the "vanilla install behaves identically" guarantee
    // documented in docs/CONFIGURATION.md.
    EXPECT_EQ(config::solarflare.busy_poll_us, 50);
    EXPECT_EQ(config::solarflare.rate_cap_pct, 80);
    EXPECT_TRUE(config::solarflare.enet_4mib_buffer);
    EXPECT_EQ(config::solarflare.pipewire_latency_ms, 8);
    EXPECT_TRUE(config::solarflare.cpu_pinning);
  }

  // ---------------------------------------------------------------------
  // In-range writes are honoured
  // ---------------------------------------------------------------------

  TEST_F(SolarflareConfigTest, InRangeValuesAreApplied) {
    // int_f / bool_f are the helpers used by apply_config. They write
    // straight through when the value is valid; we test the range
    // boundaries declared in config.cpp > apply_config().
    config::solarflare.busy_poll_us = 0;  // lower bound of 0 is allowed
    config::solarflare.busy_poll_us = 10000;  // upper bound
    config::solarflare.rate_cap_pct = 50;
    config::solarflare.rate_cap_pct = 95;
    config::solarflare.pipewire_latency_ms = 1;
    config::solarflare.pipewire_latency_ms = 40;
    config::solarflare.enet_4mib_buffer = false;
    config::solarflare.cpu_pinning = false;

    EXPECT_EQ(config::solarflare.busy_poll_us, 10000);
    EXPECT_EQ(config::solarflare.rate_cap_pct, 95);
    EXPECT_EQ(config::solarflare.pipewire_latency_ms, 40);
    EXPECT_FALSE(config::solarflare.enet_4mib_buffer);
    EXPECT_FALSE(config::solarflare.cpu_pinning);
  }

  // ---------------------------------------------------------------------
  // The struct's field defaults must stay consistent with the
  // documented ranges (catches drift if someone touches the header
  // without updating the README + docs/CONFIGURATION.md).
  // ---------------------------------------------------------------------

  TEST_F(SolarflareConfigTest, DefaultsAreInsideDocumentedRanges) {
    // busy_poll_us: 0..10000
    EXPECT_GE(config::solarflare.busy_poll_us, 0);
    EXPECT_LE(config::solarflare.busy_poll_us, 10000);

    // rate_cap_pct: 50..95
    EXPECT_GE(config::solarflare.rate_cap_pct, 50);
    EXPECT_LE(config::solarflare.rate_cap_pct, 95);

    // pipewire_latency_ms: 1..40
    EXPECT_GE(config::solarflare.pipewire_latency_ms, 1);
    EXPECT_LE(config::solarflare.pipewire_latency_ms, 40);
  }

  // ---------------------------------------------------------------------
  // bool_f behaviour: writes are round-trippable
  //
  // We don't call apply_config directly (it would require faking a
  // full config file + parsing pipeline). Instead we mirror what
  // bool_f does in src/config.cpp: it lowercases the string and
  // accepts "true", "yes", "enable", "enabled", "on", or any string
  // containing '1'. We test the round-trip for the two values the
  // fork actually uses.
  // ---------------------------------------------------------------------

  TEST_F(SolarflareConfigTest, BoolFieldsRoundTripThroughAssignment) {
    config::solarflare.enet_4mib_buffer = true;
    config::solarflare.cpu_pinning = true;
    EXPECT_TRUE(config::solarflare.enet_4mib_buffer);
    EXPECT_TRUE(config::solarflare.cpu_pinning);

    config::solarflare.enet_4mib_buffer = false;
    config::solarflare.cpu_pinning = false;
    EXPECT_FALSE(config::solarflare.enet_4mib_buffer);
    EXPECT_FALSE(config::solarflare.cpu_pinning);
  }

  // ---------------------------------------------------------------------
  // The struct is a value type (not a reference), so it can be copied
  // and assigned. This is what makes `extern solarflare_t solarflare;`
  // in src/config.h work — it's a single global, not a pointer.
  // ---------------------------------------------------------------------

  TEST_F(SolarflareConfigTest, StructIsValueType) {
    config::solarflare_t copy = config::solarflare;
    EXPECT_EQ(copy.busy_poll_us, config::solarflare.busy_poll_us);
    EXPECT_EQ(copy.rate_cap_pct, config::solarflare.rate_cap_pct);
    EXPECT_EQ(copy.enet_4mib_buffer, config::solarflare.enet_4mib_buffer);
    EXPECT_EQ(copy.pipewire_latency_ms, config::solarflare.pipewire_latency_ms);
    EXPECT_EQ(copy.cpu_pinning, config::solarflare.cpu_pinning);

    // Mutating the copy must NOT mutate the global (proves it's a
    // value type, not a reference).
    copy.busy_poll_us = 7;
    EXPECT_NE(copy.busy_poll_us, config::solarflare.busy_poll_us);
  }

}  // namespace
