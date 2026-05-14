// Copyright (c) 2026, Michael Grier
//
// On Windows with the MSVC toolchain, embed an `asInvoker` manifest in every
// test binary so that Windows UAC name-heuristics do not require elevation on
// binaries whose names contain "setup", "install", etc.
// (The `copy_render_setup` integration-test binary would otherwise be blocked
// without administrator rights.)

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=asInvoker.manifest");

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "msvc" {
        return;
    }

    // Resolve the absolute path to the manifest so the linker can find it
    // regardless of the working directory at link time.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest = std::path::Path::new(&manifest_dir).join("asInvoker.manifest");

    // /MANIFEST:EMBED  — embed the generated manifest inside the PE image.
    // /MANIFESTINPUT   — merge our asInvoker.manifest into the generated one.
    // Both flags apply only to test binaries (rustc-link-arg-tests).
    println!(
        "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
}
