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

    // `$ORIGIN` first so a packaged bundle finds the library beside itself.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    let root = std::env::var("CINDER_CEF_ROOT").unwrap_or_else(|_| {
        format!("{}/cef", std::env::var("HOME").unwrap_or_default())
    });
    let release = PathBuf::from(root).join("Release");
    if release.is_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", release.display());
    }
}
