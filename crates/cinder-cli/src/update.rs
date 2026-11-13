//! Update check against the project's published releases.
//!
//! The check is a plain HTTPS GET of the public releases list, run on its own
//! thread so the window never waits for the network. Nothing is installed
//! behind the user's back: the status strip announces the new version, and a
//! click on it downloads the package, checks it against the release's
//! `SHA256SUMS`, and hands the local file to `apt-get` through `pkexec`, which
//! asks the user for authorisation in the usual desktop dialog.

use std::path::{Path, PathBuf};
use std::process::Command as Proc;

/// The repository releases are published from.
pub const REPO: &str = "scorpionlegend/cinder";

/// One published release, reduced to what the updater needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Version without the leading `v`, e.g. `0.1.0-pre.6`.
    pub version: String,
    /// The tag as published, e.g. `v0.1.0-pre.6`.
    pub tag: String,
    /// The release page, used when the package cannot be installed here.
    pub page: String,
    /// Download URL of the package for this machine's architecture.
    pub package: Option<String>,
    /// Download URL of the release's `SHA256SUMS`.
    pub sums: Option<String>,
}

/// The version this binary was built as. Release builds carry their tag; a
/// local build falls back to the crate version.
pub fn current() -> String {
    match option_env!("CINDER_RELEASE_TAG") {
        Some(tag) if !tag.is_empty() => tag.trim_start_matches('v').to_string(),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Ask GitHub for the newest release and return it if it is newer than
/// `current`. Returns `Ok(None)` when this build is up to date.
pub fn check(current: &str) -> Result<Option<Release>, String> {
    let body = curl(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page=30"
    ))?;
    let mut best: Option<Release> = None;
    for release in parse(&body) {
        if version_cmp(&release.version, current) == std::cmp::Ordering::Greater {
            let newer = match &best {
                Some(b) => version_cmp(&release.version, &b.version) == std::cmp::Ordering::Greater,
                None => true,
            };
            if newer {
                best = Some(release);
            }
        }
    }
    Ok(best)
}

/// Download, verify and install a release. Blocks; run it off the UI thread.
pub fn install(release: &Release) -> Result<(), String> {
    let Some(package) = release.package.as_ref() else {
        return Err(format!(
            "release {} has no package for this architecture; see {}",
            release.tag, release.page
        ));
    };
    let name = package.rsplit('/').next().unwrap_or("cinder.deb");
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(name);
    curl_to(package, &path)?;

    if let Some(sums) = release.sums.as_ref() {
        let text = curl(sums)?;
        verify(&path, name, &text)?;
    } else {
        return Err(format!(
            "release {} publishes no SHA256SUMS; install it by hand from {}",
            release.tag, release.page
        ));
    }

    let apt = which("apt-get").ok_or_else(|| "apt-get is not installed".to_string())?;
    let pkexec = which("pkexec").ok_or_else(|| {
        format!(
            "pkexec is not installed; install the package by hand: sudo apt install {}",
            path.display()
        )
    })?;
    let status = Proc::new(pkexec)
        .arg(apt)
        .args(["install", "-y", "--no-install-recommends"])
        .arg(&path)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .status()
        .map_err(|e| format!("could not start the installer: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the installer exited with {status}; install by hand: sudo apt install {}",
            path.display()
        ))
    }
}

/// Where downloaded packages are staged.
fn cache_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| "no cache directory: HOME is unset".to_string())?;
    Ok(base.join("cinder").join("updates"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn curl(url: &str) -> Result<String, String> {
    let output = Proc::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: cinder-updater",
            url,
        ])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("response is not text: {e}"))
}

fn curl_to(url: &str, path: &Path) -> Result<(), String> {
    let status = Proc::new("curl")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("download failed: {status}"))
    }
}

/// Compare the file's digest with the line for it in `SHA256SUMS`.
fn verify(path: &Path, name: &str, sums: &str) -> Result<(), String> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let digest = parts.next()?;
            let listed = parts.next()?.trim_start_matches('*');
            (listed == name).then(|| digest.to_ascii_lowercase())
        })
        .ok_or_else(|| format!("{name} is not listed in SHA256SUMS"))?;
    let output = Proc::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("sha256sum: {e}"))?;
    if !output.status.success() {
        return Err("sha256sum failed".to_string());
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        let _ = std::fs::remove_file(path);
        Err(format!("checksum mismatch for {name}"))
    }
}

/// Pull releases out of the GitHub JSON without a JSON dependency: the fields
/// wanted are flat strings, and each release object starts at its `tag_name`.
fn parse(body: &str) -> Vec<Release> {
    let mut releases = Vec::new();
    let marks: Vec<usize> = body
        .match_indices("\"tag_name\"")
        .map(|(at, _)| at)
        .collect();
    for (n, &start) in marks.iter().enumerate() {
        let end = marks.get(n + 1).copied().unwrap_or(body.len());
        // Skip the marker itself so `string_after` reads the value, not the key.
        let chunk = &body[start + "\"tag_name\"".len()..end];
        let Some(tag) = string_after(chunk) else {
            continue;
        };
        // `html_url` precedes `tag_name` in a release object, so the page for
        // this tag is the last release URL before the marker, not after it.
        let before = &body[marks.get(n.wrapping_sub(1)).copied().unwrap_or(0)..start];
        let page = strings_after_field(before, "\"html_url\"")
            .into_iter()
            .rfind(|url| url.contains("/releases/tag/"))
            .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/tag/{tag}"));
        let mut package = None;
        let mut sums = None;
        for url in strings_after_field(chunk, "\"browser_download_url\"") {
            if url.ends_with("SHA256SUMS") {
                sums = Some(url);
            } else if url.ends_with(&format!("_{}.deb", deb_arch())) {
                package = Some(url);
            }
        }
        releases.push(Release {
            version: tag.trim_start_matches('v').to_string(),
            tag,
            page,
            package,
            sums,
        });
    }
    releases
}

/// The Debian architecture suffix for this machine.
fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Every JSON string after each occurrence of `field`.
fn strings_after_field(chunk: &str, field: &str) -> Vec<String> {
    chunk
        .split(field)
        .skip(1)
        .filter_map(string_after)
        .collect()
}

/// The first quoted string in `text`, starting at its opening quote.
fn string_after(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Compare two versions of the form `1.2.3` or `1.2.3-pre.4`.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a_core, a_pre) = split_pre(a);
    let (b_core, b_pre) = split_pre(b);
    let core = a_core.cmp(&b_core);
    if core != Ordering::Equal {
        return core;
    }
    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(&b),
    }
}

fn split_pre(version: &str) -> (Vec<u64>, Option<u64>) {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numbers = core
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    let pre = pre.map(|pre| {
        pre.rsplit('.')
            .next()
            .and_then(|last| last.parse().ok())
            .unwrap_or(0)
    });
    (numbers, pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_by_core_then_prerelease() {
        use std::cmp::Ordering;
        assert_eq!(version_cmp("0.1.0-pre.6", "0.1.0-pre.5"), Ordering::Greater);
        assert_eq!(version_cmp("0.1.0-pre.5", "0.1.0-pre.5"), Ordering::Equal);
        assert_eq!(version_cmp("0.1.0", "0.1.0-pre.9"), Ordering::Greater);
        assert_eq!(version_cmp("0.2.0", "0.1.9"), Ordering::Greater);
        assert_eq!(version_cmp("0.1.0-pre.10", "0.1.0-pre.9"), Ordering::Greater);
    }

    #[test]
    fn parse_picks_the_package_and_sums() {
        let body = r#"[
          {"tag_name":"v0.1.0-pre.6","html_url":"https://github.com/x/y/releases/tag/v0.1.0-pre.6",
           "assets":[
             {"browser_download_url":"https://example/cinder_0.1.0-pre.6_amd64.deb"},
             {"browser_download_url":"https://example/cinder_0.1.0-pre.6_arm64.deb"},
             {"browser_download_url":"https://example/SHA256SUMS"}]},
          {"tag_name":"v0.1.0-pre.5","html_url":"https://github.com/x/y/releases/tag/v0.1.0-pre.5",
           "assets":[]}
        ]"#;
        let releases = parse(body);
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].version, "0.1.0-pre.6");
        assert_eq!(releases[0].tag, "v0.1.0-pre.6");
        assert_eq!(
            releases[0].sums.as_deref(),
            Some("https://example/SHA256SUMS")
        );
        let package = releases[0].package.clone().unwrap_or_default();
        assert!(package.ends_with(&format!("_{}.deb", deb_arch())), "{package}");
        assert!(releases[1].package.is_none());
    }
}
