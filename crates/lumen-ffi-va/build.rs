//! Generates libva bindings into `OUT_DIR` from the system headers.
//!
//! Two dynamic-loader modules are generated; the crate never links libva:
//! - `va_core.rs` (`VaCore`): every libva entry point, loaded from `libva.so.2`.
//!   It also carries all types and constants.
//! - `va_drm.rs` (`VaDrm`): `vaGetDisplayDRM`, which `libva-drm.so.2` exports.

use std::fmt::Write as _;
use std::{env, path::PathBuf};

const SYSTEM_INCLUDE: &str = "/usr/include";
const HEADERS: &[&str] = &[
    "va/va.h",
    "va/va_backend.h",
    "va/va_drm.h",
    "va/va_drmcommon.h",
    "va/va_enc_h264.h",
    "va/va_enc_hevc.h",
    "va/va_version.h",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for header in HEADERS {
        println!("cargo:rerun-if-changed={SYSTEM_INCLUDE}/{header}");
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut wrapper = String::new();
    for header in HEADERS {
        writeln!(wrapper, "#include <{header}>").expect("writing to a String cannot fail");
    }
    let base = || {
        bindgen::Builder::default()
            .header_contents("lumen_va_wrapper.h", &wrapper)
            .generate_comments(false)
            .derive_default(true)
            // Enum constants keep their plain libva names (`VAProfileH264High`).
            .prepend_enum_name(false)
            // Every unsafe operation inside the generated `unsafe fn`s gets its own block.
            .wrap_unsafe_ops(true)
            // A missing symbol fails `VaCore::new` once, instead of panicking at call time.
            .dynamic_link_require_all(true)
    };

    base()
        .dynamic_library_name("VaCore")
        .allowlist_function("va[A-Z].*")
        .blocklist_function("vaGetDisplayDRM")
        .allowlist_type("VA.*")
        .blocklist_type("VADriver.*|VADisplayContext.*")
        .allowlist_var("VA_.*")
        .generate()
        .expect("bindgen failed for libva core")
        .write_to_file(out_dir.join("va_core.rs"))
        .expect("write va_core.rs");

    base()
        .dynamic_library_name("VaDrm")
        .allowlist_function("vaGetDisplayDRM")
        .generate()
        .expect("bindgen failed for libva-drm")
        .write_to_file(out_dir.join("va_drm.rs"))
        .expect("write va_drm.rs");
}
