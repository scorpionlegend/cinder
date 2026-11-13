//! Build script for the CEF-backed adapter (L0/L1).
//!
//! Compiles the C shim against the CEF headers and links the CEF runtime for the
//! target OS. The CEF binary distribution is located through `CINDER_CEF_ROOT`
//! (default `$HOME/cef`); the C compiler is chosen by the `cc` crate (`CC`,
//! `CFLAGS`, MSVC on Windows).
//!
//! Nothing here downloads anything: point `CINDER_CEF_ROOT` at an extracted
//! `cef_binary_*` directory for the target platform.
//!
//! Link layout per OS:
//!   Linux   `Release/libcef.so`, rpath `$ORIGIN` so a shipped bundle finds the
//!           copy next to the executable, plus the absolute dev path.
//!   macOS   `Release/Chromium Embedded Framework.framework`, rpath
//!           `@executable_path/../Frameworks`.
//!   Windows `Release/libcef.lib` import library; `libcef.dll` and
//!           `chrome_elf.dll` are copied next to the built executable.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=shim/cinder_cef_shim.c");
    println!("cargo:rerun-if-changed=shim/cinder_cef_shim.h");
    println!("cargo:rerun-if-env-changed=CINDER_CEF_ROOT");
    println!("cargo:rerun-if-env-changed=CINDER_CEF_API_VERSION");

    let cef_root = cef_root();
    let release_dir = cef_root.join("Release");
    let include_dir = cef_root.join("include");
    if !include_dir.join("cef_version.h").exists() {
        panic!(
            "CEF headers not found under {}. Set CINDER_CEF_ROOT to an extracted \
             cef_binary_* directory for this platform.",
            cef_root.display()
        );
    }

    let api_version = env::var("CINDER_CEF_API_VERSION").unwrap_or_else(|_| "15800".to_string());

    let mut build = cc::Build::new();
    build
        .file("shim/cinder_cef_shim.c")
        .include(&cef_root)
        .define("CEF_API_VERSION", api_version.as_str())
        .opt_level(2)
        .warnings(false);
    build.compile("cinder_cef_shim");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            println!("cargo:rustc-link-search=native={}", release_dir.display());
            println!("cargo:rustc-link-lib=dylib=libcef");
            copy_runtime_dlls(&release_dir);
        }
        "macos" => {
            // CEF ships as a framework on macOS. `rustc-link-lib` and
            // `rustc-link-search` are recorded in the crate metadata and
            // propagate to the final binary link; `rustc-link-arg` does not,
            // which left every CEF symbol undefined when linking `cinder`.
            println!(
                "cargo:rustc-link-search=framework={}",
                release_dir.display()
            );
            println!("cargo:rustc-link-lib=framework=Chromium Embedded Framework");
            // The shim resizes the browser view with objc_msgSend.
            println!("cargo:rustc-link-lib=dylib=objc");
        }
        _ => {
            println!("cargo:rustc-link-search=native={}", release_dir.display());
            println!("cargo:rustc-link-lib=dylib=cef");
            // `$ORIGIN` first: a packaged bundle carries libcef.so beside the
            // executable. The absolute path keeps `cargo run` working in a dev
            // tree where the CEF distribution lives outside the repo.
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", release_dir.display());
        }
    }

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

/// On Windows the loader resolves `libcef.dll` next to the executable, so a
/// `cargo run` in a dev tree needs the DLLs copied out of the CEF distribution.
fn copy_runtime_dlls(release_dir: &Path) {
    let Some(profile_dir) = profile_dir() else {
        return;
    };
    for name in ["libcef.dll", "chrome_elf.dll", "vk_swiftshader.dll", "vulkan-1.dll"] {
        let src = release_dir.join(name);
        if !src.exists() {
            continue;
        }
        let dst = profile_dir.join(name);
        if dst.exists() {
            continue;
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            println!("cargo:warning=could not copy {name} next to the executable: {e}");
        }
    }
}

/// `OUT_DIR` is `<target>/<triple>/<profile>/build/<pkg>-<hash>/out`; the
/// directory holding the built executable is three levels up.
fn profile_dir() -> Option<PathBuf> {
    let out = PathBuf::from(env::var("OUT_DIR").ok()?);
    out.ancestors().nth(3).map(Path::to_path_buf)
}
