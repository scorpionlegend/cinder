//! Per-platform bundle layout.
//!
//! Each function returns the archive members for one target, with paths relative
//! to the archive root. Nothing is copied to a staging directory: entries point
//! at their source files and the archive writers stream them, so packaging a
//! 300 MB runtime does not need 300 MB of scratch space.

use std::io;
use std::path::{Path, PathBuf};

use crate::archive::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub triple: String,
    pub os: Os,
    pub arch: &'static str,
}

impl Target {
    pub fn parse(triple: &str) -> Result<Self, String> {
        let os = if triple.contains("linux") {
            Os::Linux
        } else if triple.contains("darwin") {
            Os::Macos
        } else if triple.contains("windows") {
            Os::Windows
        } else {
            return Err(format!("unsupported target triple: {triple}"));
        };
        let arch = if triple.starts_with("x86_64") {
            "x86_64"
        } else if triple.starts_with("aarch64") {
            "aarch64"
        } else {
            return Err(format!("unsupported architecture in {triple}"));
        };
        Ok(Self {
            triple: triple.to_string(),
            os,
            arch,
        })
    }

    /// Debian architecture name, used in the `.deb` control file.
    pub fn deb_arch(&self) -> &'static str {
        match self.arch {
            "x86_64" => "amd64",
            _ => "arm64",
        }
    }

    pub fn exe(&self, name: &str) -> String {
        match self.os {
            Os::Windows => format!("{name}.exe"),
            _ => name.to_string(),
        }
    }
}

const LINUX_RUNTIME: &[&str] = &[
    "libcef.so",
    "icudtl.dat",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "resources.pak",
    "v8_context_snapshot.bin",
    "libvk_swiftshader.so",
    "libvulkan.so.1",
    "vk_swiftshader_icd.json",
    "chrome-sandbox",
];

/// Files the Windows CEF distribution keeps in `Release/`.
///
/// `d3dcompiler_47.dll` is what ANGLE compiles its D3D11 shaders with; without
/// it Chromium falls back to software rendering.
const WINDOWS_RELEASE: &[&str] = &[
    "libcef.dll",
    "chrome_elf.dll",
    "d3dcompiler_47.dll",
    "v8_context_snapshot.bin",
    "vk_swiftshader.dll",
    "vulkan-1.dll",
    "vk_swiftshader_icd.json",
];

/// Files the Windows CEF distribution keeps in `Resources/`.
///
/// Unlike Linux, where these sit next to `libcef.so`, the Windows archive
/// splits them into a separate directory. Reading them from `Release/` silently
/// produced a bundle with no ICU data and no pak files, which starts but cannot
/// render a page.
const WINDOWS_RESOURCES: &[&str] = &[
    "icudtl.dat",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "resources.pak",
];

fn locales(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("pak") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn notices(cef_root: &Path, version: &str, cef_version: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let text = format!(
        "Cinder {version}\n\
         \n\
         This package bundles the Chromium Embedded Framework (CEF) {cef_version},\n\
         which embeds Chromium. CEF and Chromium are licensed under their own terms;\n\
         see licenses/CEF-LICENSE.txt and licenses/CEF-CREDITS.html.\n\
         \n\
         Cinder's own code is licensed under Apache-2.0.\n"
    );
    out.push(Entry::bytes("THIRD_PARTY_NOTICES.txt", text, 0o644));
    for (src, dst) in [
        ("LICENSE.txt", "licenses/CEF-LICENSE.txt"),
        ("CREDITS.html", "licenses/CEF-CREDITS.html"),
    ] {
        let path = cef_root.join(src);
        if path.exists() {
            out.push(Entry::file(dst, path, 0o644));
        }
    }
    out
}

/// Copy `libcef.so` and drop its debug sections. The pinned CEF beta ships an
/// unstripped 1.4 GB library; stripping takes it to roughly 280 MB without
/// touching code. The manifest records that this happened.
pub fn prepare_libcef(release_dir: &Path, work_dir: &Path, strip: bool) -> io::Result<PathBuf> {
    let src = release_dir.join("libcef.so");
    if !strip {
        return Ok(src);
    }
    std::fs::create_dir_all(work_dir)?;
    let dst = work_dir.join("libcef.so");
    std::fs::copy(&src, &dst)?;
    let status = std::process::Command::new("strip")
        .arg("--strip-unneeded")
        .arg(&dst)
        .status();
    match status {
        Ok(s) if s.success() => Ok(dst),
        _ => {
            eprintln!("warning: strip failed, packaging the unstripped libcef.so");
            Ok(src)
        }
    }
}

pub struct BundleInput<'a> {
    pub target: &'a Target,
    pub version: &'a str,
    pub bin_dir: &'a Path,
    pub cef_root: &'a Path,
    pub cef_version: &'a str,
    pub libcef: &'a Path,
}

/// Root directory name inside the archive, e.g. `cinder-0.1.0-x86_64-unknown-linux-gnu`.
pub fn root_name(input: &BundleInput<'_>) -> String {
    format!("cinder-{}-{}", input.version, input.target.triple)
}

pub fn linux_entries(input: &BundleInput<'_>) -> io::Result<Vec<Entry>> {
    let root = root_name(input);
    let release = input.cef_root.join("Release");
    let mut out = vec![Entry::dir(root.clone(), 0o755)];

    for bin in ["cinder", "cef-spike"] {
        let src = input.bin_dir.join(bin);
        if src.exists() {
            out.push(Entry::file(format!("{root}/{bin}"), src, 0o755));
        }
    }
    out.push(Entry::file(
        format!("{root}/libcef.so"),
        input.libcef,
        0o755,
    ));
    for name in LINUX_RUNTIME.iter().filter(|n| **n != "libcef.so") {
        let src = release.join(name);
        if src.exists() {
            let mode = if name.ends_with(".so") || *name == "chrome-sandbox" {
                0o755
            } else {
                0o644
            };
            out.push(Entry::file(format!("{root}/{name}"), src, mode));
        }
    }
    let locale_dir = release.join("locales");
    if locale_dir.is_dir() {
        out.push(Entry::dir(format!("{root}/locales"), 0o755));
        for pak in locales(&locale_dir)? {
            let name = pak.file_name().unwrap().to_string_lossy().into_owned();
            out.push(Entry::file(format!("{root}/locales/{name}"), pak, 0o644));
        }
    }
    out.extend(notices(input.cef_root, input.version, input.cef_version));
    Ok(out)
}

pub fn windows_entries(input: &BundleInput<'_>) -> io::Result<Vec<Entry>> {
    let root = root_name(input);
    let release = input.cef_root.join("Release");
    let resources = input.cef_root.join("Resources");
    let mut out = vec![Entry::dir(root.clone(), 0o755)];

    for bin in ["cinder", "cef-spike"] {
        let src = input.bin_dir.join(input.target.exe(bin));
        if src.exists() {
            out.push(Entry::file(
                format!("{root}/{}", input.target.exe(bin)),
                src,
                0o755,
            ));
        }
    }
    for name in WINDOWS_RELEASE {
        let src = release.join(name);
        if src.exists() {
            out.push(Entry::file(format!("{root}/{name}"), src, 0o644));
        }
    }
    for name in WINDOWS_RESOURCES {
        let src = resources.join(name);
        if src.exists() {
            out.push(Entry::file(format!("{root}/{name}"), src, 0o644));
        }
    }
    let locale_dir = resources.join("locales");
    if locale_dir.is_dir() {
        out.push(Entry::dir(format!("{root}/locales"), 0o755));
        for pak in locales(&locale_dir)? {
            let name = pak.file_name().unwrap().to_string_lossy().into_owned();
            out.push(Entry::file(format!("{root}/locales/{name}"), pak, 0o644));
        }
    }
    out.extend(notices(input.cef_root, input.version, input.cef_version));
    Ok(out)
}

/// `Cinder.app` with the CEF framework in `Contents/Frameworks`.
pub fn macos_entries(input: &BundleInput<'_>) -> io::Result<Vec<Entry>> {
    let root = root_name(input);
    let app = format!("{root}/Cinder.app/Contents");
    let release = input.cef_root.join("Release");
    let mut out = vec![
        Entry::dir(root.clone(), 0o755),
        Entry::dir(format!("{root}/Cinder.app"), 0o755),
        Entry::dir(app.clone(), 0o755),
        Entry::dir(format!("{app}/MacOS"), 0o755),
        Entry::dir(format!("{app}/Frameworks"), 0o755),
        Entry::dir(format!("{app}/Resources"), 0o755),
    ];

    for bin in ["cinder", "cef-spike"] {
        let src = input.bin_dir.join(bin);
        if src.exists() {
            out.push(Entry::file(format!("{app}/MacOS/{bin}"), src, 0o755));
        }
    }

    let framework = release.join("Chromium Embedded Framework.framework");
    if framework.is_dir() {
        walk(&framework, &format!("{app}/Frameworks/Chromium Embedded Framework.framework"), &mut out)?;
    }
    // CEF resolves icudtl.dat and the .pak files next to the framework's
    // Resources directory; keep a copy in the app bundle as well.
    for name in ["icudtl.dat", "chrome_100_percent.pak", "chrome_200_percent.pak", "resources.pak", "v8_context_snapshot.bin"] {
        let src = release.join(name);
        if src.exists() {
            out.push(Entry::file(format!("{app}/Resources/{name}"), src, 0o644));
        }
    }
    let locale_dir = release.join("locales");
    if locale_dir.is_dir() {
        out.push(Entry::dir(format!("{app}/Resources/locales"), 0o755));
        for pak in locales(&locale_dir)? {
            let name = pak.file_name().unwrap().to_string_lossy().into_owned();
            out.push(Entry::file(format!("{app}/Resources/locales/{name}"), pak, 0o644));
        }
    }
    out.push(Entry::bytes(
        format!("{app}/Info.plist"),
        info_plist(input.version),
        0o644,
    ));
    out.extend(notices(input.cef_root, input.version, input.cef_version));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<Entry>) -> io::Result<()> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .collect();
    children.sort();
    for path in children {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            out.push(Entry::dir(rel.clone(), 0o755));
            walk(&path, &rel, out)?;
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            out.push(Entry::symlink(rel, target.to_string_lossy().into_owned()));
        } else {
            let mode = if is_executable(&meta) { 0o755 } else { 0o644 };
            out.push(Entry::file(rel, path, mode));
        }
    }
    Ok(())
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Cinder</string>
  <key>CFBundleDisplayName</key><string>Cinder</string>
  <key>CFBundleIdentifier</key><string>io.cinder.Cinder</string>
  <key>CFBundleExecutable</key><string>cinder</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
"#
    )
}

/// Remap the Linux bundle into Debian's filesystem layout.
pub fn deb_data_entries(linux: &[Entry], root: &str) -> Vec<Entry> {
    let mut out = vec![
        Entry::dir("./usr", 0o755),
        Entry::dir("./usr/lib", 0o755),
        Entry::dir("./usr/lib/cinder", 0o755),
        Entry::dir("./usr/bin", 0o755),
        Entry::dir("./usr/share", 0o755),
        Entry::dir("./usr/share/applications", 0o755),
        Entry::dir("./usr/share/metainfo", 0o755),
        Entry::dir("./usr/share/doc", 0o755),
        Entry::dir("./usr/share/doc/cinder", 0o755),
    ];
    for entry in linux {
        if entry.kind == crate::archive::Kind::Dir {
            continue;
        }
        let Some(rest) = entry.path.strip_prefix(&format!("{root}/")) else {
            continue;
        };
        let mut mapped = entry.clone();
        mapped.path = format!("./usr/lib/cinder/{rest}");
        out.push(mapped);
    }
    out.push(Entry::symlink("./usr/bin/cinder", "../lib/cinder/cinder"));
    out.push(Entry::bytes(
        "./usr/share/applications/cinder.desktop",
        crate::DESKTOP_FILE,
        0o644,
    ));
    out.push(Entry::bytes(
        "./usr/share/metainfo/io.cinder.Cinder.metainfo.xml",
        crate::APPSTREAM_FILE,
        0o644,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cinder-dist-test-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
            std::fs::write(&path, rel.as_bytes()).expect("write file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn windows_input<'a>(
        scratch: &'a Scratch,
        target: &'a Target,
        bin_dir: &'a Path,
        cef_root: &'a Path,
        libcef: &'a Path,
    ) -> BundleInput<'a> {
        let _ = scratch;
        BundleInput {
            target,
            version: "0.1.0-test",
            bin_dir,
            cef_root,
            cef_version: "test",
            libcef,
        }
    }

    /// The Windows CEF archive keeps the pak files and ICU data in `Resources/`,
    /// not next to the DLLs. Reading them from `Release/` produced a bundle that
    /// started but could not render, so this pins the split layout.
    #[test]
    fn windows_bundle_takes_resources_from_the_resources_directory() {
        let scratch = Scratch::new("win-resources");
        let cef = scratch.0.join("cef");
        for name in WINDOWS_RELEASE {
            scratch.file(&format!("cef/Release/{name}"));
        }
        for name in WINDOWS_RESOURCES {
            scratch.file(&format!("cef/Resources/{name}"));
        }
        scratch.file("cef/Resources/locales/en-US.pak");
        scratch.file("cef/Resources/locales/de.pak");
        let bin = scratch.file("bin/cinder.exe");
        let bin_dir = bin.parent().unwrap().to_path_buf();

        let target = Target::parse("x86_64-pc-windows-msvc").unwrap();
        let libcef = cef.join("Release/libcef.dll");
        let input = windows_input(&scratch, &target, &bin_dir, &cef, &libcef);
        let entries = windows_entries(&input).expect("windows entries");

        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let root = root_name(&input);
        for name in WINDOWS_RESOURCES {
            let want = format!("{root}/{name}");
            assert!(paths.contains(&want.as_str()), "missing {want} in {paths:?}");
        }
        assert!(paths.contains(&format!("{root}/locales/en-US.pak").as_str()));
        assert!(paths.contains(&format!("{root}/locales/de.pak").as_str()));
        assert!(paths.contains(&format!("{root}/cinder.exe").as_str()));

        // Every file entry must point at something that exists; a silently
        // skipped source is how the missing-resources bug stayed invisible.
        for entry in &entries {
            if let Some(source) = entry.source.as_ref() {
                assert!(source.exists(), "entry {} has no source", entry.path);
            }
        }
    }

    #[test]
    fn windows_bundle_skips_absent_optional_files() {
        let scratch = Scratch::new("win-optional");
        let cef = scratch.0.join("cef");
        scratch.file("cef/Release/libcef.dll");
        scratch.file("cef/Resources/icudtl.dat");
        let bin = scratch.file("bin/cinder.exe");
        let bin_dir = bin.parent().unwrap().to_path_buf();

        let target = Target::parse("x86_64-pc-windows-msvc").unwrap();
        let libcef = cef.join("Release/libcef.dll");
        let input = windows_input(&scratch, &target, &bin_dir, &cef, &libcef);
        let entries = windows_entries(&input).expect("windows entries");
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();

        assert!(paths.iter().any(|p| p.ends_with("/libcef.dll")));
        assert!(paths.iter().any(|p| p.ends_with("/icudtl.dat")));
        assert!(!paths.iter().any(|p| p.ends_with("/d3dcompiler_47.dll")));
        assert!(!paths.iter().any(|p| p.contains("/locales/")));
    }
}
