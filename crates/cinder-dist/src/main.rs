//! `cinder-dist`: assemble a downloadable Cinder package for one target.
//!
//! ```text
//! cinder-dist --target x86_64-unknown-linux-gnu \
//!             --cef-root ~/cef --out dist
//! ```
//!
//! It bundles the built binaries with the pinned CEF runtime, writes the
//! platform archives, and emits a manifest recording exactly what went in.

mod archive;
mod bundle;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use archive::Entry;
use bundle::{BundleInput, Os, Target};

pub const DESKTOP_FILE: &str = include_str!("../../../packaging/linux/cinder.desktop");

pub const APPSTREAM_FILE: &str =
    include_str!("../../../packaging/linux/io.cinder.Cinder.metainfo.xml");

/// Direct dependencies of the bundled Chromium runtime: exactly the SONAMEs
/// `libcef.so` lists in its `DT_NEEDED` entries, mapped to Debian package
/// names. Transitive libraries are deliberately not listed; apt resolves them.
///
/// Packages renamed for the 64-bit `time_t` transition (`libasound2t64` and
/// friends, Ubuntu 24.04+) are declared as alternatives so one control file
/// installs on both older and newer releases. `libunistring2` is not listed at
/// all: it no longer exists in Ubuntu 26.04 and was only ever a transitive
/// dependency of libgnutls30.
///
/// `libc6 (>= 2.34)` is the highest glibc symbol the binaries actually use
/// (Ubuntu 22.04's level). Without the floor apt would happily install on an
/// older release and the failure would only appear at launch as a missing
/// symbol.
const DEB_DEPENDS: &str = "libc6 (>= 2.34), libgcc-s1, libx11-6, libxcb1, libxcomposite1, \
libxdamage1, libxext6, libxfixes3, libxrandr2, libxkbcommon0, libgbm1, libnss3, \
libnspr4, libdbus-1-3, libexpat1, libpango-1.0-0, libcairo2, libudev1, \
libcups2 | libcups2t64, libatk1.0-0 | libatk1.0-0t64, \
libatk-bridge2.0-0 | libatk-bridge2.0-0t64, libatspi2.0-0 | libatspi2.0-0t64, \
libasound2 | libasound2t64, libglib2.0-0 | libglib2.0-0t64";

struct Args {
    target: String,
    cef_root: PathBuf,
    bin_dir: Option<PathBuf>,
    out: PathBuf,
    version: String,
    formats: Option<Vec<String>>,
    strip_cef: bool,
    cef_version: Option<String>,
}

fn usage() -> &'static str {
    "usage: cinder-dist --target <triple> [--cef-root <dir>] [--bin-dir <dir>]\n\
     \x20                  [--out <dir>] [--version <v>] [--formats tar.gz,zip,deb]\n\
     \x20                  [--no-strip-cef] [--cef-version <v>]"
}

fn parse_args() -> Result<Args, String> {
    let mut target = None;
    let mut cef_root = None;
    let mut bin_dir = None;
    let mut out = PathBuf::from("dist");
    let mut version = None;
    let mut formats = None;
    let mut strip_cef = true;
    let mut cef_version = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--target" => target = Some(value("--target")?),
            "--cef-root" => cef_root = Some(PathBuf::from(value("--cef-root")?)),
            "--bin-dir" => bin_dir = Some(PathBuf::from(value("--bin-dir")?)),
            "--out" => out = PathBuf::from(value("--out")?),
            "--version" => version = Some(value("--version")?),
            "--formats" => {
                formats = Some(
                    value("--formats")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }
            "--cef-version" => cef_version = Some(value("--cef-version")?),
            "--no-strip-cef" => strip_cef = false,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        target: target.ok_or("--target is required")?,
        cef_root: cef_root.unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("cef")
        }),
        bin_dir,
        out,
        version: version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        formats,
        strip_cef,
        cef_version,
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cinder-dist: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let target = Target::parse(&args.target)?;
    let bin_dir = args.bin_dir.clone().unwrap_or_else(|| default_bin_dir(&target));
    if !bin_dir.is_dir() {
        return Err(format!(
            "binary directory {} does not exist; build with `cargo build --release --target {}` first",
            bin_dir.display(),
            target.triple
        ));
    }
    if !args.cef_root.join("Release").is_dir() {
        return Err(format!(
            "{} has no Release directory; point --cef-root at an extracted cef_binary_* tree",
            args.cef_root.display()
        ));
    }
    std::fs::create_dir_all(&args.out).map_err(|e| format!("creating {}: {e}", args.out.display()))?;

    let cef_version = args.cef_version.clone().unwrap_or_else(|| {
        std::fs::read_to_string(args.cef_root.join("include/cef_version.h"))
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|l| l.contains("CEF_VERSION "))
                    .and_then(|l| l.split('"').nth(1).map(str::to_string))
            })
            .unwrap_or_else(|| "unknown".to_string())
    });

    let work = args.out.join(".work");
    let libcef = if target.os == Os::Linux {
        bundle::prepare_libcef(&args.cef_root.join("Release"), &work, args.strip_cef)
            .map_err(|e| format!("preparing libcef.so: {e}"))?
    } else {
        args.cef_root.join("Release/libcef.so")
    };

    let input = BundleInput {
        target: &target,
        version: &args.version,
        bin_dir: &bin_dir,
        cef_root: &args.cef_root,
        cef_version: &cef_version,
        libcef: &libcef,
    };

    let entries = match target.os {
        Os::Linux => bundle::linux_entries(&input),
        Os::Macos => bundle::macos_entries(&input),
        Os::Windows => bundle::windows_entries(&input),
    }
    .map_err(|e| format!("assembling bundle: {e}"))?;

    let formats = args.formats.clone().unwrap_or_else(|| match target.os {
        Os::Linux => vec!["tar.gz".into(), "deb".into()],
        Os::Macos => vec!["tar.gz".into()],
        Os::Windows => vec!["zip".into()],
    });

    let stem = format!("cinder-{}-{}", args.version, target.triple);
    let mtime = 1_700_000_000u64; // fixed: keeps archives byte-identical between runs
    let mut produced: Vec<(String, u64, String)> = Vec::new();

    for format in &formats {
        let path = match format.as_str() {
            "tar.gz" => {
                let p = args.out.join(format!("{stem}.tar.gz"));
                archive::write_tar_gz(&entries, &p, mtime)
                    .map_err(|e| format!("writing {}: {e}", p.display()))?;
                p
            }
            "zip" => {
                let p = args.out.join(format!("{stem}.zip"));
                archive::write_zip(&entries, &p, mtime)
                    .map_err(|e| format!("writing {}: {e}", p.display()))?;
                p
            }
            "deb" => {
                let p = args.out.join(format!(
                    "cinder_{}_{}.deb",
                    args.version,
                    target.deb_arch()
                ));
                write_deb(&entries, &p, &args, &target, mtime)?;
                p
            }
            other => return Err(format!("unknown format: {other}")),
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let sum = archive::sha256_file(&path).map_err(|e| format!("hashing {}: {e}", path.display()))?;
        produced.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            size,
            sum,
        ));
    }

    let manifest = manifest_json(&args, &target, &cef_version, &entries, &produced, &libcef);
    let manifest_path = args.out.join(format!("{stem}.manifest.json"));
    std::fs::write(&manifest_path, manifest)
        .map_err(|e| format!("writing {}: {e}", manifest_path.display()))?;

    println!("target   : {}", target.triple);
    println!("cef      : {cef_version}");
    println!("files    : {}", entries.len());
    for (name, size, sum) in &produced {
        println!("artifact : {name}  {} MiB  sha256 {}", size / 1_048_576, &sum[..16]);
    }
    println!("manifest : {}", manifest_path.display());
    Ok(())
}

fn default_bin_dir(target: &Target) -> PathBuf {
    let triple = PathBuf::from("target").join(&target.triple).join("release");
    if triple.is_dir() {
        triple
    } else {
        PathBuf::from("target/release")
    }
}

fn write_deb(
    linux: &[Entry],
    path: &Path,
    args: &Args,
    target: &Target,
    mtime: u64,
) -> Result<(), String> {
    let root = format!("cinder-{}-{}", args.version, target.triple);
    let data = bundle::deb_data_entries(linux, &root);
    let installed_kb: u64 = data
        .iter()
        .filter(|e| e.kind == archive::Kind::File)
        .map(|e| e.size)
        .sum::<u64>()
        / 1024;

    let control = format!(
        "Package: cinder\n\
         Version: {}\n\
         Architecture: {}\n\
         Maintainer: Allen Schoepflin <alband3000@gmail.com>\n\
         Installed-Size: {}\n\
         Depends: {}\n\
         Section: web\n\
         Priority: optional\n\
         Homepage: https://github.com/scorpionlegend/cinder\n\
         Description: Cinder web browser\n\
         \x20 Cinder is a web browser built on the Chromium engine through CEF.\n\
         \x20 The components Cinder owns are written in Rust; the bundled Chromium\n\
         \x20 runtime is not.\n",
        args.version,
        target.deb_arch(),
        installed_kb,
        DEB_DEPENDS
    );

    let copyright = "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n\
         Upstream-Name: Cinder\n\
         Source: https://github.com/scorpionlegend/cinder\n\
         \n\
         Files: *\n\
         Copyright: Allen Schoepflin\n\
         License: Apache-2.0\n\
         \n\
         Files: usr/lib/cinder/libcef.so usr/lib/cinder/*.pak usr/lib/cinder/locales/*\n\
         Copyright: The Chromium Embedded Framework authors\n\
         License: BSD-3-Clause\n\
         \n"
        .to_string();

    let mut control_entries = vec![
        Entry::bytes("./control", control, 0o644),
        Entry::bytes("./copyright", copyright, 0o644),
    ];
    control_entries.sort_by(|a, b| a.path.cmp(&b.path));

    let control_tar = archive::tar_gz_bytes(&control_entries, mtime)
        .map_err(|e| format!("building control.tar.gz: {e}"))?;
    let data_tar =
        archive::tar_gz_bytes(&data, mtime).map_err(|e| format!("building data.tar.gz: {e}"))?;

    archive::write_ar(
        &[
            ("debian-binary", b"2.0\n".to_vec()),
            ("control.tar.gz", control_tar),
            ("data.tar.gz", data_tar),
        ],
        path,
    )
    .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(())
}

fn manifest_json(
    args: &Args,
    target: &Target,
    cef_version: &str,
    entries: &[Entry],
    produced: &[(String, u64, String)],
    libcef: &Path,
) -> String {
    let mut files = String::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            files.push_str(",\n");
        }
        files.push_str(&format!(
            "    {{\"path\": \"{}\", \"size\": {}}}",
            entry.path, entry.size
        ));
    }
    let mut artifacts = String::new();
    for (i, (name, size, sum)) in produced.iter().enumerate() {
        if i > 0 {
            artifacts.push_str(",\n");
        }
        artifacts.push_str(&format!(
            "    {{\"name\": \"{name}\", \"bytes\": {size}, \"sha256\": \"{sum}\"}}"
        ));
    }
    format!(
        "{{\n  \"product\": \"cinder\",\n  \"version\": \"{}\",\n  \"target\": \"{}\",\n  \
         \"cef_version\": \"{cef_version}\",\n  \"libcef_source\": \"{}\",\n  \
         \"libcef_stripped\": {},\n  \"files\": [\n{files}\n  ],\n  \
         \"artifacts\": [\n{artifacts}\n  ]\n}}\n",
        args.version,
        target.triple,
        libcef.display(),
        args.strip_cef && target.os == Os::Linux,
    )
}

#[allow(dead_code)]
fn ensure_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}
