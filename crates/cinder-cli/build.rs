//! Gives the `cinder` binary an rpath for `libcef.so` when the `cef` feature is
//! on.
//!
//! `cinder-adapter-cef` sets its own rpath, but that only applies to targets in
//! that crate. A binary in `cinder-cli` links `libcef.so` too, so without this
//! it fails at load time with "cannot open shared object file" before any of
//! its own code — including the runtime staging — can run.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CINDER_CEF_ROOT");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CEF");

    if std::env::var_os("CARGO_FEATURE_CEF").is_none() {
        return;
    }

    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");

    // `$ORIGIN` first so a packaged bundle finds the library beside itself.
    // macOS has no `$ORIGIN`; its bundle layout is handled below.
    if !macos {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }

    let root = std::env::var("CINDER_CEF_ROOT").unwrap_or_else(|_| {
        format!("{}/cef", std::env::var("HOME").unwrap_or_default())
    });
    let release = PathBuf::from(root).join("Release");
    if release.is_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", release.display());
    }

    // On macOS the runtime lives in `Cinder.app/Contents/Frameworks`, which is
    // `../Frameworks` relative to the executable.
    if macos {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    }
}
