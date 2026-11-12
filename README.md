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
| `crates/cinder-cli` | `cinder` binary that drives the orchestrator against the mock engine |

`unsafe` is forbidden in every crate (`unsafe_code = "forbid"`), so the boundary
cannot quietly grow a native escape hatch before an audited L0 crate exists.

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
