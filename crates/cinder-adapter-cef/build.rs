//! Build script for the CEF-backed adapter (L0/L1).
//!
//! Compiles the C shim against the CEF headers and links `libcef.so`. The CEF
//! binary distribution is located through `CINDER_CEF_ROOT` (default
//! `$HOME/cef`); the C compiler through `CINDER_CC` (default `zig cc`).
//!
//! Nothing here downloads anything: point `CINDER_CEF_ROOT` at an extracted
//! `cef_binary_*_linux64_*` directory.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=shim/cinder_cef_shim.c");
    println!("cargo:rerun-if-changed=shim/cinder_cef_shim.h");
    println!("cargo:rerun-if-env-changed=CINDER_CEF_ROOT");
    println!("cargo:rerun-if-env-changed=CINDER_CC");

    let cef_root = cef_root();
    let release_dir = cef_root.join("Release");
    let include_dir = cef_root.join("include");
    if !include_dir.join("cef_version.h").exists() {
        panic!(
            "CEF headers not found under {}. Set CINDER_CEF_ROOT to an extracted \
             cef_binary_*_linux64_* directory.",
            cef_root.display()
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out_dir.join("cinder_cef_shim.o");
    let lib = out_dir.join("libcinder_cef_shim.a");

    let cc = cc_command();
    let mut cmd = Command::new(&cc[0]);
    cmd.args(&cc[1..])
        .arg("cc")
        .arg("-I")
        .arg(&cef_root)
        .arg("-DCEF_API_VERSION=15800")
        .arg("-O2")
        .arg("-fPIC")
        .arg("-c")
        .arg("shim/cinder_cef_shim.c")
        .arg("-o")
        .arg(&obj);
    run(cmd, "compile shim");

    let mut ar = Command::new(&cc[0]);
    ar.args(&cc[1..]).arg("ar").arg("rcs").arg(&lib).arg(&obj);
    run(ar, "archive shim");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=cinder_cef_shim");
    println!("cargo:rustc-link-search=native={}", release_dir.display());
    println!("cargo:rustc-link-lib=dylib=cef");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", release_dir.display());
    println!("cargo:rustc-env=CINDER_CEF_ROOT={}", cef_root.display());
}

fn cef_root() -> PathBuf {
    match env::var("CINDER_CEF_ROOT") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("cef")
        }
    }
}

/// `CINDER_CC` may be a full command line, e.g. `zig cc` or `/usr/bin/clang`.
fn cc_command() -> Vec<String> {
    match env::var("CINDER_CC") {
        Ok(v) if !v.is_empty() => v.split_whitespace().map(str::to_string).collect(),
        _ => vec!["zig".to_string()],
    }
}

fn run(mut cmd: Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to {what}: {e}"));
    if !status.success() {
        panic!("failed to {what}: {status}");
    }
}

#[allow(dead_code)]
fn exists(p: &Path) -> bool {
    p.exists()
}
