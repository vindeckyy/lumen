//! Check whether a newer Lumen build exists.
//!
//! The manifest is fetched over plain HTTP and verified with the pinned Ed25519 key, so the
//! transport does not have to be trusted: a mirror that answers with another manifest cannot
//! forge one.

use std::process::ExitCode;

use lumen_update_check::{floor::Floor, version};

const USAGE: &str = "lumen-update-check - check whether a newer Lumen build exists

Usage: lumen-update-check --url <url> --key <64 hex chars> [options]

Options:
  --url <url>        Where the manifest is (http://host/path)
  --key <hex>        The pinned Ed25519 public key, 64 hex characters
  --channel <name>   The channel to accept (default stable)
  --floor <n>        The serial already seen; a manifest below it is refused
  --current <ver>    The version that is running; the result says whether it is newer
  --help             Print this text
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut url = String::new();
    let mut key = String::new();
    let mut channel = "stable".to_owned();
    let mut floor = Floor::new();
    let mut current = String::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--url" => match value("--url") {
                Ok(v) => url = v,
                Err(e) => {
                    eprintln!("lumen-update-check: {e}");
                    return ExitCode::from(2);
                }
            },
            "--key" => match value("--key") {
                Ok(v) => key = v,
                Err(e) => {
                    eprintln!("lumen-update-check: {e}");
                    return ExitCode::from(2);
                }
            },
            "--channel" => match value("--channel") {
                Ok(v) => channel = v,
                Err(e) => {
                    eprintln!("lumen-update-check: {e}");
                    return ExitCode::from(2);
                }
            },
            "--floor" => {
                let v = match value("--floor") {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("lumen-update-check: {e}");
                        return ExitCode::from(2);
                    }
                };
                let Ok(serial) = v.parse::<u64>() else {
                    eprintln!("lumen-update-check: the floor {v:?} is not a number");
                    return ExitCode::from(2);
                };
                floor = Floor::at(serial);
            }
            "--current" => match value("--current") {
                Ok(v) => current = v,
                Err(e) => {
                    eprintln!("lumen-update-check: {e}");
                    return ExitCode::from(2);
                }
            },
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("lumen-update-check: unknown option {other}");
                print!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    if url.is_empty() || key.is_empty() {
        eprintln!("lumen-update-check: --url and --key are needed");
        print!("{USAGE}");
        return ExitCode::from(2);
    }
    let Some(bytes) = decode_hex(&key) else {
        eprintln!("lumen-update-check: the key is not hex");
        return ExitCode::from(2);
    };
    let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) else {
        eprintln!("lumen-update-check: the key is not 32 bytes");
        return ExitCode::from(2);
    };

    match lumen_update_check::check(&url, &key, &channel, &floor) {
        Ok(update) => {
            let newer = current.is_empty() || version::is_newer(&update.manifest.version, &current);
            println!(
                "version={} channel={} serial={} url={} newer_than_current={newer}",
                update.manifest.version,
                update.manifest.channel,
                update.serial,
                update.manifest.url
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lumen-update-check: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The bytes of a hex text.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}
