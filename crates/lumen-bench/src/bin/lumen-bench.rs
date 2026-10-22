//! The `lumen-bench` command line tool.
//!
//! Two subcommands:
//!
//! - `report` merges the host and the client counters of one run into the run
//!   report that `artifacts/m1/e2e.json` holds.
//! - `rig-selftest` exercises the photodiode harness arithmetic against
//!   synthetic camera frames, so the harness is verified without the rig.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lumen_bench::e2e;
use lumen_bench::report::RunReport;
use lumen_bench::trigger;

/// Usage text for the command line.
const USAGE: &str = "\
lumen-bench - Lumen measurement harness

Usage: lumen-bench <command> [options]

Commands:
  report        Merge a host run and a client run into one report
  rig-selftest  Verify the photodiode harness arithmetic without the rig

report options:
  --host <path>        Host counters JSON
  --client <path>      Client counters JSON
  --input <path>       An already merged report to assert against
  --out <path>         Where the merged report goes (default artifacts/m1/e2e.json)
  --set <key=value>    A configuration entry; may be repeated
  --assert <expr>      A threshold, for example frames_decoded>1700; may be repeated
  --provisional        Stamp the report provisional even if it was not

Assertion names are the counter fields (frames_sent, frames_decoded,
frames_presented, dropped_frames, decode_errors, fec_recoveries, retransmits,
packets_lost, packets_received, bitrate_kbps) and the stage percentiles
p50_<stage>_ms, p90_<stage>_ms, p99_<stage>_ms, max_<stage>_ms and
mean_<stage>_ms, where <stage> is capture, encode, send, recv, decode, present
or end_to_end. The present stage is the time from the end of the decode to
the presentation; end_to_end is the whole pipeline, from the compositor's
frame timestamp to the presentation.

rig-selftest options:
  --out <path>         Where the simulated fixture goes (default artifacts/m1/rig-selftest.json)
  --frames <n>         Frames to simulate (default 30)
  --trigger-delay <us> Delay between the host presenting and the diode firing
  --client-latency <us> Delay between the diode firing and the camera seeing it

  -h, --help           Print this text
";

/// Run the tool.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    match command.as_str() {
        "report" => report(&args[1..]),
        "rig-selftest" => rig_selftest(&args[1..]),
        "-h" | "--help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("lumen-bench: unknown command {other}");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Merge the host and client counters into one report.
fn report(args: &[String]) -> ExitCode {
    let mut host_path: Option<PathBuf> = None;
    let mut client_path: Option<PathBuf> = None;
    let mut input: Option<PathBuf> = None;
    let mut out = PathBuf::from("artifacts/m1/e2e.json");
    let mut configuration: Vec<(String, String)> = Vec::new();
    let mut assertions: Vec<String> = Vec::new();
    let mut force_provisional = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |_flag: &str| -> Option<String> { it.next().cloned() };
        match arg.as_str() {
            "--host" => host_path = value("--host").map(PathBuf::from),
            "--client" => client_path = value("--client").map(PathBuf::from),
            "--input" => input = value("--input").map(PathBuf::from),
            "--assert" => {
                let Some(v) = value("--assert") else {
                    eprintln!("lumen-bench: --assert needs an expression");
                    return ExitCode::from(2);
                };
                assertions.push(v);
            }
            "--provisional" => force_provisional = true,
            "--out" => {
                if let Some(v) = value("--out") {
                    out = PathBuf::from(v);
                }
            }
            "--set" => {
                let Some(v) = value("--set") else {
                    eprintln!("lumen-bench: --set needs a key=value");
                    return ExitCode::from(2);
                };
                let Some((k, val)) = v.split_once('=') else {
                    eprintln!("lumen-bench: --set needs a key=value, got {v}");
                    return ExitCode::from(2);
                };
                configuration.push((k.to_string(), val.to_string()));
            }
            other => {
                eprintln!("lumen-bench: unknown option {other}");
                return ExitCode::from(2);
            }
        }
    }

    // An already merged report can be asserted against on its own.
    let report = if let Some(input) = &input {
        match RunReport::read_json(input) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("lumen-bench: cannot read {}: {e}", input.display());
                return ExitCode::FAILURE;
            }
        }
    } else {
        let (Some(host_path), Some(client_path)) = (host_path, client_path) else {
            eprintln!("lumen-bench: report needs --host and --client, or --input");
            return ExitCode::from(2);
        };
        let host = match e2e::read_host(&host_path) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("lumen-bench: cannot read the host report: {e}");
                return ExitCode::FAILURE;
            }
        };
        let client = match e2e::read_client(&client_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("lumen-bench: cannot read the client report: {e}");
                return ExitCode::FAILURE;
            }
        };
        e2e::merge(&host, &client, &configuration)
    };

    let mut report = report;
    if force_provisional && !report.provisional {
        report.provisional = true;
        report.provisional_reason = Some("stamped provisional on the command line".to_string());
    }

    let mut failed = false;
    for expression in &assertions {
        match check(&report, expression) {
            Ok(()) => println!("  ok   {expression}"),
            Err(why) => {
                println!("  FAIL {expression}: {why}");
                failed = true;
            }
        }
    }

    if let Err(e) = report.write_json(&out) {
        eprintln!("lumen-bench: cannot write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    print!("{}", report.summary());
    eprintln!("lumen-bench: wrote {}", out.display());
    if failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Check one assertion against a report.
///
/// The expression is a name, an operator and a number, for example
/// `frames_decoded>1700` or `p99_end_to_end_ms<=12`.
fn check(report: &RunReport, expression: &str) -> Result<(), String> {
    let (name, operator, number) = split_expression(expression)?;
    let Some(actual) = resolve(report, name) else {
        return Err(format!("unknown name {name}"));
    };
    let ok = match operator {
        ">" => actual > number,
        ">=" => actual >= number,
        "<" => actual < number,
        "<=" => actual <= number,
        "==" => (actual - number).abs() < f64::EPSILON,
        "!=" => (actual - number).abs() >= f64::EPSILON,
        other => return Err(format!("unknown operator {other}")),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{actual} is not {operator} {number}"))
    }
}

/// Split an assertion into its name, operator and number.
fn split_expression(expression: &str) -> Result<(&str, &str, f64), String> {
    for operator in [">=", "<=", "==", "!=", ">", "<"] {
        if let Some((name, number)) = expression.split_once(operator) {
            let number = number
                .trim()
                .parse::<f64>()
                .map_err(|e| format!("{number:?} is not a number: {e}"))?;
            return Ok((name.trim(), operator, number));
        }
    }
    Err(format!("{expression:?} has no operator"))
}

/// Resolve an assertion name to a number.
fn resolve(report: &RunReport, name: &str) -> Option<f64> {
    let counters = &report.counters;
    let counter = match name {
        "frames_sent" => Some(counters.frames_sent as f64),
        "frames_decoded" => Some(counters.frames_decoded as f64),
        "frames_presented" => Some(counters.frames_presented as f64),
        "dropped_frames" => Some(counters.dropped_frames as f64),
        "decode_errors" => Some(counters.decode_errors as f64),
        "fec_recoveries" => Some(counters.fec_recoveries as f64),
        "retransmits" => Some(counters.retransmits as f64),
        "packets_lost" => Some(counters.packets_lost as f64),
        "packets_received" => Some(counters.packets_received as f64),
        "bitrate_kbps" => Some(report.bitrate_kbps),
        _ => None,
    };
    if counter.is_some() {
        return counter;
    }
    // A percentile of a stage, in milliseconds.
    for (prefix, pick) in [
        ("p50_", 0usize),
        ("p90_", 1),
        ("p99_", 2),
        ("max_", 3),
        ("mean_", 4),
    ] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        let Some(stage) = rest.strip_suffix("_ms") else {
            continue;
        };
        let Some(stats) = report.stages.get(stage) else {
            continue;
        };
        let value = match pick {
            0 => stats.p50_us,
            1 => stats.p90_us,
            2 => stats.p99_us,
            3 => stats.max_us,
            _ => stats.mean_us,
        };
        return Some(value as f64 / 1000.0);
    }
    None
}

/// Verify the harness arithmetic without the rig.
fn rig_selftest(args: &[String]) -> ExitCode {
    let mut out = PathBuf::from("artifacts/m1/rig-selftest.json");
    let mut frames = 30usize;
    let mut trigger_delay_us = 1000u64;
    let mut client_latency_us = 5000u64;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |_flag: &str| -> Option<String> { it.next().cloned() };
        match arg.as_str() {
            "--out" => {
                if let Some(v) = value("--out") {
                    out = PathBuf::from(v);
                }
            }
            "--frames" => {
                let Some(v) = value("--frames") else {
                    eprintln!("lumen-bench: --frames needs a value");
                    return ExitCode::from(2);
                };
                frames = v.parse().unwrap_or(0);
            }
            "--trigger-delay" => {
                let Some(v) = value("--trigger-delay") else {
                    eprintln!("lumen-bench: --trigger-delay needs a value");
                    return ExitCode::from(2);
                };
                trigger_delay_us = v.parse().unwrap_or(0);
            }
            "--client-latency" => {
                let Some(v) = value("--client-latency") else {
                    eprintln!("lumen-bench: --client-latency needs a value");
                    return ExitCode::from(2);
                };
                client_latency_us = v.parse().unwrap_or(0);
            }
            other => {
                eprintln!("lumen-bench: unknown option {other}");
                return ExitCode::from(2);
            }
        }
    }

    let simulated = match trigger::simulate(frames, trigger_delay_us, client_latency_us) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lumen-bench: cannot simulate the rig: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = trigger::write_fixture(&out, &simulated) {
        eprintln!("lumen-bench: cannot write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    let read_back = match trigger::read_fixture(&out) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lumen-bench: cannot read {}: {e}", out.display());
            return ExitCode::FAILURE;
        }
    };

    // The fixture must survive the round trip.
    if read_back.glass_to_glass_us != simulated.glass_to_glass_us {
        eprintln!(
            "lumen-bench: the fixture lost the latency: {} became {}",
            simulated.glass_to_glass_us, read_back.glass_to_glass_us
        );
        return ExitCode::FAILURE;
    }
    if read_back.observations.len() != simulated.observations.len() {
        eprintln!("lumen-bench: the fixture lost observations");
        return ExitCode::FAILURE;
    }

    // Every observation must give the same glass-to-glass latency.
    for observation in &read_back.observations {
        let measured = match trigger::glass_to_glass(observation) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("lumen-bench: the observation is inconsistent: {e}");
                return ExitCode::FAILURE;
            }
        };
        if measured != read_back.glass_to_glass_us {
            eprintln!(
                "lumen-bench: observation gave {measured} but the run says {}",
                read_back.glass_to_glass_us
            );
            return ExitCode::FAILURE;
        }
    }

    // The barcode in a synthetic camera image must read back.
    let (image, width, height) = trigger::synthetic_camera_image(7, 123_456);
    let (frame_index, timestamp_us) =
        match trigger::read_barcode(&image, width, height, lumen_bench::pattern::barcode_width()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("lumen-bench: the barcode did not read back: {e}");
                return ExitCode::FAILURE;
            }
        };
    if frame_index != 7 || timestamp_us != 123_456 {
        eprintln!(
            "lumen-bench: the barcode read back {frame_index}/{timestamp_us}, expected 7/123_456"
        );
        return ExitCode::FAILURE;
    }

    println!(
        "lumen-bench: rig self-test passed: {} observations, glass-to-glass {} us, barcode round trip ok",
        read_back.observations.len(),
        read_back.glass_to_glass_us
    );
    eprintln!("lumen-bench: wrote {}", Path::new(&out).display());
    ExitCode::SUCCESS
}
