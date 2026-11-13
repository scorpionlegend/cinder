//! Locating and staging the CEF runtime.
//!
//! Chromium does not read `icudtl.dat` from `resources_dir_path`; it looks for
//! it next to the `libcef.so` that the dynamic loader actually mapped. A CEF
//! binary distribution keeps its runtime split across `Resources/` and
//! `Release/`, so a binary built against the distribution root gets a
//! `libcef.so` with no ICU data beside it and dies with
//! `Invalid file descriptor to ICU data received`.
//!
//! [`stage`] merges both directories into one place and [`ensure_loaded`]
//! re-executes the process with that directory on `LD_LIBRARY_PATH`, which puts
//! the library and its data in the same directory.

use std::path::{Path, PathBuf};

/// Environment variable that marks a process as already re-executed, so a
/// failure to stage the runtime cannot turn into an infinite exec loop.
const STAGED_ENV: &str = "CINDER_CEF_STAGED";

/// Where the CEF runtime lives.
///
/// `CINDER_CEF_ROOT` wins. Otherwise an installed bundle is used, which keeps
/// the runtime next to the executable; only a checkout falls back to `~/cef`.
/// `HOME` is unset on Windows, so the bundle check has to come first there.
pub fn root() -> PathBuf {
    if let Ok(explicit) = std::env::var("CINDER_CEF_ROOT") {
        return PathBuf::from(explicit);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        if has_runtime(&dir) {
            return dir;
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join("cef")
}

/// Whether `dir` is a flat bundle with both the ICU data and a CEF library.
///
/// A build directory can hold `icudtl.dat` and the pak files without the
/// library, and staging that copy leaves CEF unable to load at all, so the
/// check requires the library too.
fn has_runtime(dir: &Path) -> bool {
    dir.join("icudtl.dat").is_file()
        && (dir.join("libcef.so").is_file()
            || dir.join("libcef.dll").is_file()
            || dir.join("Chromium Embedded Framework.framework").is_dir())
}

/// Merge the CEF runtime under `root` into `dest`, returning `dest`.
///
/// A CEF distribution keeps its files in `Resources/` and `Release/`; a Cinder
/// bundle keeps them flat next to the executable. Both layouts are staged, so
/// the same code works from a checkout and from an installed bundle.
///
/// Files are copied rather than symlinked: Chromium opens the ICU data through
/// a file descriptor and rejects a symlink there. The result is cached behind a
/// `.ready` marker so repeated calls in one process are cheap.
pub fn stage(root: &Path, dest: &Path) -> PathBuf {
    // The marker records which root produced the copy: a different root must
    // not silently reuse the previous one's files.
    let marker = dest.join(".ready");
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(&root.to_string_lossy()) {
        return dest.to_path_buf();
    }
    let _ = std::fs::remove_dir_all(dest);
    let _ = std::fs::create_dir_all(dest);
    let mut nested = false;
    let mut failed = 0usize;
    for sub in ["Resources", "Release"] {
        let Ok(entries) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        nested = true;
        failed += copy_entries(entries, dest);
    }
    if !nested {
        if let Ok(entries) = std::fs::read_dir(root) {
            failed += copy_entries(entries, dest);
        }
    }
    if failed > 0 {
        eprintln!(
            "cinder: {failed} runtime files could not be staged into {}",
            dest.display()
        );
    }
    // A half-copied runtime fails later as a confusing Chromium error, so only
    // mark the copy usable when the files Chromium needs are actually there.
    if failed == 0 && staged_ok(dest) {
        let _ = std::fs::write(&marker, root.to_string_lossy().as_bytes());
    } else {
        let _ = std::fs::remove_file(&marker);
    }
    dest.to_path_buf()
}

/// Whether a staged directory holds the files Chromium refuses to start
/// without: the ICU data and the CEF library itself.
fn staged_ok(dest: &Path) -> bool {
    let non_empty = |path: PathBuf| {
        std::fs::metadata(path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    };
    let library = ["libcef.so", "libcef.dll", "Chromium Embedded Framework"]
        .iter()
        .any(|name| non_empty(dest.join(name)));
    non_empty(dest.join("icudtl.dat")) && library
}

fn copy_entries(entries: std::fs::ReadDir, dest: &Path) -> usize {
    let mut failed = 0;
    for entry in entries.flatten() {
        let target = dest.join(entry.file_name());
        let source = entry.path();
        let result = if source.is_dir() {
            copy_dir(&source, &target)
        } else {
            std::fs::copy(&source, &target).map(|_| ())
        };
        if result.is_err() {
            failed += 1;
        }
    }
    failed
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Re-execute the current binary with `runtime` on `LD_LIBRARY_PATH` so the
/// loader picks `libcef.so` out of the staged directory. Returns only when the
/// runtime is already in place; otherwise it never returns.
///
/// Only meaningful on Linux, where `LD_LIBRARY_PATH` is honoured. On other
/// platforms the loader uses the rpath the build script set, so this is a
/// no-op.
pub fn ensure_loaded(runtime: &Path) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let current = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let already = current
        .split(':')
        .any(|entry| !entry.is_empty() && Path::new(entry) == runtime);
    if already || std::env::var_os(STAGED_ENV).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(
            "LD_LIBRARY_PATH",
            format!("{}:{}", runtime.display(), current),
        )
        .env(STAGED_ENV, "1")
        .status();
    std::process::exit(status.ok().and_then(|s| s.code()).unwrap_or(1));
}

/// Stage the runtime under `root` and make sure this process loaded it.
/// Returns the staged directory, which holds `icudtl.dat`, the `.pak` files and
/// `libcef.so` together.
pub fn prepare(root: &Path) -> PathBuf {
    let dest = std::env::temp_dir().join("cinder-cef-runtime");
    let staged = stage(root, &dest);
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        let paks = std::fs::read_dir(staged.join("locales"))
            .map(|dir| dir.count())
            .unwrap_or(0);
        eprintln!(
            "window trace: runtime staged from {} icu={} locales={}",
            root.display(),
            staged.join("icudtl.dat").exists(),
            paks
        );
    }
    ensure_loaded(&staged);
    staged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_needs_both_icu_and_the_library() {
        let dir = scratch("has-runtime");
        std::fs::write(dir.join("icudtl.dat"), b"icu").unwrap();
        assert!(!has_runtime(&dir), "icu alone is not a bundle");
        std::fs::write(dir.join("libcef.so"), b"cef").unwrap();
        assert!(has_runtime(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cinder-stage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A CEF distribution keeps its files in `Resources/` and `Release/`.
    #[test]
    fn stages_a_nested_distribution() {
        let root = scratch("nested-root");
        let dest = scratch("nested-dest");
        std::fs::create_dir_all(root.join("Resources/locales")).unwrap();
        std::fs::create_dir_all(root.join("Release")).unwrap();
        std::fs::write(root.join("Resources/icudtl.dat"), b"icu").unwrap();
        std::fs::write(root.join("Resources/locales/en-US.pak"), b"pak").unwrap();
        std::fs::write(root.join("Release/libcef.so"), b"so").unwrap();

        let staged = stage(&root, &dest);

        assert!(staged.join("icudtl.dat").exists());
        assert!(staged.join("locales/en-US.pak").exists());
        assert!(staged.join("libcef.so").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }

    /// A Cinder bundle keeps the same files flat next to the executable, which
    /// is what an installed Windows build looks like.
    #[test]
    fn stages_a_flat_bundle() {
        let root = scratch("flat-root");
        let dest = scratch("flat-dest");
        std::fs::create_dir_all(root.join("locales")).unwrap();
        std::fs::write(root.join("icudtl.dat"), b"icu").unwrap();
        std::fs::write(root.join("locales/en-US.pak"), b"pak").unwrap();
        std::fs::write(root.join("libcef.dll"), b"dll").unwrap();

        let staged = stage(&root, &dest);

        assert!(staged.join("icudtl.dat").exists());
        assert!(staged.join("locales/en-US.pak").exists());
        assert!(staged.join("libcef.dll").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
