# Cinder

A web browser with a Chromium engine; every Cinder-owned component is written in
Rust.

Chromium stays a non-Rust C++ engine. "Pure Rust" applies to the components
Cinder owns — shell, tabs and windows, session orchestration, profile and policy,
permissions, update orchestration — which reach the engine only through a narrow,
typed adapter boundary.

## Status

Early implementation. The typed adapter, the orchestrator, the mock engine and
the CEF-backed adapter (L0/L1) exist, and the CEF off-screen render spike passes.
`cinder` opens a real window: a toolbar with back, forward, reload and an address
field, and the page itself is a native CEF child window, so Chromium paints and
handles input directly. The embedding path is Linux/X11 for now (XWayland on a
Wayland session); the macOS and Windows bundles build and open the shell but do
not yet host a page. Still missing: tabs, bookmarks, downloads, settings and
the full Chrome-style chrome.

| Crate | Role |
| --- | --- |
| `crates/cinder-adapter` | L2 typed API: IDs, commands, events, one-shot decisions, view state machine, policy snapshots, input sanitizers |
| `crates/cinder-adapter-mock` | Scripted in-memory engine implementing the same traits |
| `crates/cinder-core` | L3 orchestrator: windows, tabs, history, decision routing, session snapshot/restore, shutdown escalation |
| `crates/cinder-adapter-cef` | L0/L1 CEF adapter: flat C shim over the CEF C API, plus the `cef-spike` binary |
| `crates/cinder-cli` | `cinder` binary that drives the orchestrator against the mock engine |
| `crates/cinder-dist` | Packager: bundles the binaries with the pinned CEF runtime and writes the platform archives |

`unsafe` is forbidden in every crate except `cinder-adapter-cef`, the audited
native boundary (L0/L1), so the boundary cannot quietly grow elsewhere.

## Downloads

Every supported target gets a self-contained package: the Cinder binaries plus
the pinned CEF runtime, so nothing has to be installed separately.

| Target | Artifact |
| --- | --- |
| Windows x86_64 | `cinder-<version>-x86_64-pc-windows-msvc.msi` (installer), `cinder-<version>-x86_64-pc-windows-msvc-setup.exe` (installer), and the portable `.zip` |
| macOS x86_64 / arm64 | `cinder-<version>-<triple>.dmg` (drag-to-Applications image) and the portable `.tar.gz` containing `Cinder.app` |
| Linux x86_64 / arm64 | `cinder-<version>-<triple>.tar.gz` and `cinder_<version>_<arch>.deb` |
| Fedora / RHEL / openSUSE | `cinder-<version>-1.<arch>.rpm`, built from the same Linux bundle |

The portable tarball is the fallback that works on any glibc distribution; the
`.deb` and `.rpm` only add desktop-entry and `PATH` integration. The Windows
installers put the bundle in `Program Files\Cinder` with Start Menu and
uninstall entries; the `.dmg` holds `Cinder.app` next to an `/Applications`
symlink. None of the installers are code-signed yet, so Windows SmartScreen and
macOS Gatekeeper will warn on first launch.

`crates/cinder-dist` builds the archives and the `.deb`; the installers are
assembled from the same bundle in CI — WiX 3 (`packaging/windows/cinder.wxs`)
for the `.msi`, Inno Setup (`packaging/windows/cinder.iss`) for the setup
`.exe`, and `hdiutil` (`packaging/macos/make-dmg.sh`) for the `.dmg`. Each one
is installed or mounted again in the workflow before it is uploaded.

`crates/cinder-dist` builds these. It streams the runtime straight from the CEF
distribution into the archive, so packaging does not need a staging copy:

```sh
cargo build --release -p cinder-cli -p cinder-adapter-cef
cargo run -p cinder-dist --release -- \
  --target x86_64-unknown-linux-gnu --cef-root ~/cef --out dist
```

CEF is pinned per platform in `packaging/cef-pin.json` (version, file name and
published SHA-1); `packaging/fetch-cef.py` downloads and verifies it. `.github/workflows/release.yml`
runs the matrix on five runners and publishes a GitHub Release with `SHA256SUMS`
when a `v*` tag is pushed.

Measured on the Linux x86_64 build: 237 files, `cinder-0.1.0-x86_64-unknown-linux-gnu.tar.gz`
at 164,377,098 bytes and `cinder_0.1.0_amd64.deb` at 163,748,796 bytes. The
pinned CEF beta ships an unstripped `libcef.so` of 1,492,848,720 bytes;
`cinder-dist` strips debug sections by default, which takes it to 284,777,632
bytes, and records that in the manifest (`--no-strip-cef` disables it).

## CEF spike: measured

`crates/cinder-adapter-cef` links CEF 158.0.1 through a flat C shim
(`shim/cinder_cef_shim.c`) rather than a hand-mirrored CEF vtable, so the
compiler checks the shim against the real headers on every build. The spike
creates one windowless browser, waits for a painted frame, and writes it as a
PPM.

Result of the recorded run (`CINDER_CEF_ROOT=~/cef cargo run -p cinder-adapter-cef --bin cef-spike`):

| Measurement | Value |
| --- | --- |
| CEF / Chromium | 158.0.1+g6ee3fb9+chromium-158.0.8127.18 |
| First paint | 169 ms |
| Frame | 800×600 BGRA, 1,920,000 bytes |
| Distinct colors | 845 |
| Background `#101828` | 448,404 px |
| Green block `#34d399` | 25,600 px (= 320×80, exactly the block's area) |
| Shutdown | clean, exit 0 |

Measured capabilities: off-screen rendering, load events, title reporting, URL
reporting. Not yet exercised: incognito contexts, download pause, print to PDF,
find-in-page, DevTools, request interception.

Two integration facts worth keeping: `cef_api_hash()` must be called before
`cef_execute_process()` or every CToCpp conversion reports version `-1`, and
`CEF_API_VERSION` must be pinned to match the shipped `libcef.so`.

### Running it in a restricted sandbox

`shim/socket_shim.c` is an `LD_PRELOAD` diagnostic that substitutes an AF_INET
loopback socket for the AF_UNIX one Chromium's process singleton requires. It
exists only because this sandbox denies `socket(AF_UNIX)`; **it must never
ship**, since it removes the single-instance lock. Two other sandbox
constraints: `icudtl.dat` and the `.pak` files must sit next to `libcef.so`, and
`TMPDIR` must be short enough that the singleton socket path fits in
`sun_path` (108 bytes).

## Try it

`cinder-adapter-cef` needs the CEF headers and runtime at build time, so a fresh
clone builds and tests everything else first:

```sh
cargo test --workspace --exclude cinder-adapter-cef
cargo run -p cinder-cli -- demo
```

To include the CEF crate, fetch the pinned distribution (SHA-1 verified) and
point the build at it:

```sh
python3 packaging/fetch-cef.py --platform linux64 --dest ~/cef
CINDER_CEF_ROOT=~/cef cargo test --workspace
CINDER_CEF_ROOT=~/cef cargo run -p cinder-adapter-cef --bin cef-spike
```

The `cinder` binary links the real web engine only when built with the `cef`
feature, so the default build stays free of CEF headers:

```sh
CINDER_CEF_ROOT=~/cef cargo run -p cinder-cli --features cef -- browse https://example.com/
```

That loads the page in Chromium, prints the title and URL the orchestrator
observed, and shuts the engine down. Without the feature `cinder browse` exits 2
with a message saying so.

Without `CINDER_CEF_ROOT` the CEF crate's build script stops with an explicit
error instead of silently building a stub, so a missing runtime can never be
mistaken for a working one.

The demo opens tabs, applies a policy snapshot (host blocklist, HTTPS upgrade),
answers a permission request, saves and restores a session, and shuts down —
including the escalation path for a view that refuses to close.

## Design documents

The architecture, threat model, adapter interface spec, release pipeline and
spike plan live in the project's document set (`Cinder_*.md`), not in this repo.
