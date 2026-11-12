# Cinder

A web browser with a Chromium engine; every Cinder-owned component is written in
Rust.

Chromium stays a non-Rust C++ engine. "Pure Rust" applies to the components
Cinder owns — shell, tabs and windows, session orchestration, profile and policy,
permissions, update orchestration — which reach the engine only through a narrow,
typed adapter boundary.

## Status

Early implementation of the adapter contract and the Cinder-side model. **No
native engine is linked in yet**: the CEF-backed adapter (L0/L1) is not
implemented, so nothing here renders a web page. What exists is the part that
carries forward unchanged once CEF is wired in.

| Crate | Role |
| --- | --- |
| `crates/cinder-adapter` | L2 typed API: IDs, commands, events, one-shot decisions, view state machine, policy snapshots, input sanitizers |
| `crates/cinder-adapter-mock` | Scripted in-memory engine implementing the same traits |
| `crates/cinder-core` | L3 orchestrator: windows, tabs, history, decision routing, session snapshot/restore, shutdown escalation |
| `crates/cinder-adapter-cef` | L0/L1 CEF adapter: flat C shim over the CEF C API, plus the `cef-spike` binary |
| `crates/cinder-cli` | `cinder` binary that drives the orchestrator against the mock engine |

`unsafe` is forbidden in every crate except `cinder-adapter-cef`, the audited
native boundary (L0/L1), so the boundary cannot quietly grow elsewhere.

## CEF spike: measured

`crates/cinder-adapter-cef` links CEF 158.0.1 through a flat C shim
(`shim/cinder_cef_shim.c`) rather than a hand-mirrored CEF vtable, so the
compiler checks the shim against the real headers on every build. The spike
creates one windowless browser, waits for a painted frame, and writes it as a
PPM.

Result of the recorded run (`cargo run -p cinder-adapter-cef --bin cef-spike`):

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

```sh
cargo test --workspace
cargo run -p cinder-cli -- demo
```

The demo opens tabs, applies a policy snapshot (host blocklist, HTTPS upgrade),
answers a permission request, saves and restores a session, and shuts down —
including the escalation path for a view that refuses to close.

## Design documents

The architecture, threat model, adapter interface spec, release pipeline and
spike plan live in the project's document set (`Cinder_*.md`), not in this repo.
