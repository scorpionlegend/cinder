//! End-to-end proof that `CefEngine` satisfies the `Engine` contract.
//!
//! `harness = false` on purpose: CEF must be initialized from the process main
//! thread, and `cef_execute_process` must run before anything else so CEF's
//! re-executed subprocesses exit instead of starting another test run.
//!
//! Run with:
//!   CINDER_CEF_ROOT=~/cef cargo test -p cinder-adapter-cef --test engine_e2e

use std::time::{Duration, Instant};

use cinder_adapter::command::{Command, CommandOutcome, Surface};
use cinder_adapter::engine::Engine;
use cinder_adapter::event::{Event, EventStream, LoadFailure};
use cinder_adapter::policy::PolicySnapshot;
use cinder_adapter::sanitize::Url;
use cinder_adapter_cef::{Cef, CefEngine, CefEngineConfig};

const PAGE_BODY: &str = "<html><head><title>Cinder Engine E2E</title></head><body style='margin:0;background:%23101828;color:%23f0f4ff'><h1 style='padding:24px'>Cinder engine</h1><p style='padding:0 24px'>findable needle text</p></body></html>";

/// `Url` refuses `data:` by design, so the page is written to disk and loaded
/// over `file://`.
fn page_url() -> Url {
    let path = std::env::temp_dir().join("cinder-engine-e2e.html");
    std::fs::write(&path, PAGE_BODY).expect("write page");
    Url::parse(&format!("file://{}", path.display())).expect("file url")
}

fn main() {
    if let Some(code) = Cef::execute_process() {
        std::process::exit(code);
    }
    let failures = run();
    if failures.is_empty() {
        println!("\nengine_e2e: PASS");
        std::process::exit(0);
    }
    for failure in &failures {
        println!("  FAIL {failure}");
    }
    println!("\nengine_e2e: FAIL ({} check(s))", failures.len());
    std::process::exit(1);
}

struct Check {
    failures: Vec<String>,
}

impl Check {
    fn that(&mut self, ok: bool, what: impl Into<String>) {
        let what = what.into();
        println!("  {} {}", if ok { "ok  " } else { "FAIL" }, what);
        if !ok {
            self.failures.push(what);
        }
    }
}

/// Drains the stream into a buffer so an event is never lost just because a
/// later assertion was the one waiting for it.
struct Collector {
    events: Vec<Event>,
}

impl Collector {
    fn new() -> Self {
        Self { events: Vec::new() }
    }

    fn pump(
        &mut self,
        stream: &EventStream,
        timeout: Duration,
        pred: impl Fn(&Event) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.events.iter().any(&pred) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if let Ok(message) = stream.recv_timeout(Duration::from_millis(50)) {
                if let Some(envelope) = message.event() {
                    self.events.push(envelope.event.clone());
                }
            }
        }
    }

    fn dump(&self) {
        println!("\n  -- {} event(s) observed --", self.events.len());
        for event in &self.events {
            println!("     {:?}", event);
        }
    }
}

fn config() -> CefEngineConfig {
    let resources = cinder_adapter_cef::prepare_runtime(&cinder_adapter_cef::cef_root());
    let cache = std::env::temp_dir().join("cinder-engine-e2e-cache");
    let _ = std::fs::create_dir_all(&cache);
    CefEngineConfig {
        resources_dir: resources.clone(),
        locales_dir: resources.join("locales"),
        cache_dir: cache.clone(),
        extra_args: vec![
            "--disable-gpu".into(),
            "--disable-dev-shm-usage".into(),
            "--ozone-platform=headless".into(),
            format!("--user-data-dir={}", cache.display()),
        ],
        capabilities: cinder_adapter_cef::measured_capabilities(),
    }
}

fn run() -> Vec<String> {
    let mut check = Check {
        failures: Vec::new(),
    };

    let engine = match CefEngine::new(config()) {
        Ok(engine) => engine,
        Err(e) => {
            check.that(false, format!("engine initializes: {e}"));
            return check.failures;
        }
    };
    check.that(true, "engine initializes");

    let stream = engine.subscribe().expect("first subscribe");
    check.that(engine.subscribe().is_none(), "second subscribe returns None");

    let mut events = Collector::new();
    check.that(
        events.pump(&stream, Duration::from_secs(10), |e| {
            matches!(e, Event::EngineInitialized { .. })
        }),
        "EngineInitialized arrives",
    );

    let profile = match engine.execute(Command::CreateProfile {
        path: "/tmp/cinder-e2e-profile".into(),
        incognito: false,
    }) {
        Ok(CommandOutcome::ProfileCreated(p)) => p,
        other => {
            check.that(false, format!("CreateProfile: {other:?}"));
            return check.failures;
        }
    };
    check.that(true, "CreateProfile returns ProfileCreated");

    check.that(
        engine
            .execute(Command::CreateProfile {
                path: "/tmp/cinder-e2e-incognito".into(),
                incognito: true,
            })
            .is_err(),
        "incognito profile is refused (capability is false)",
    );

    let policy = PolicySnapshot::new(7)
        .with_blocked_host("blocked.example")
        .with_https_upgrade(true);
    check.that(
        engine
            .execute(Command::InstallPolicy {
                snapshot: std::sync::Arc::new(policy),
            })
            .is_ok(),
        "InstallPolicy is accepted",
    );

    let view = match engine.execute(Command::CreateView {
        profile,
        surface: Surface::Offscreen {
            width: 800,
            height: 600,
        },
        initial_url: Some(page_url()),
    }) {
        Ok(CommandOutcome::ViewCreated(v)) => v,
        other => {
            check.that(false, format!("CreateView: {other:?}"));
            return check.failures;
        }
    };
    check.that(true, format!("CreateView returns {view}"));

    check.that(
        events.pump(&stream, Duration::from_secs(20), |e| {
            matches!(e, Event::ViewReady { .. })
        }),
        "ViewReady arrives",
    );
    check.that(
        events.pump(&stream, Duration::from_secs(20), |e| {
            matches!(e, Event::LoadFinished { .. })
        }),
        "LoadFinished arrives for the file: page",
    );
    check.that(
        events.pump(&stream, Duration::from_secs(10), |e| {
            matches!(e, Event::TitleChanged { title, .. } if title.contains("Cinder Engine E2E"))
        }),
        "TitleChanged carries the page title",
    );
    check.that(
        events.pump(&stream, Duration::from_secs(10), |e| {
            matches!(e, Event::UrlChanged { url, .. } if url.as_str().starts_with("file://"))
        }),
        "UrlChanged reports the loaded URL",
    );

    check.that(
        engine
            .execute(Command::FindInPage {
                view,
                query: "needle".into(),
                forward: true,
            })
            .is_ok(),
        "FindInPage is accepted",
    );
    check.that(
        engine
            .execute(Command::SetZoom {
                view,
                factor: 1.25,
            })
            .is_ok(),
        "SetZoom is accepted",
    );
    check.that(
        engine
            .execute(Command::Resize {
                view,
                width: 1024,
                height: 768,
                scale: 1.0,
            })
            .is_ok(),
        "Resize is accepted",
    );
    check.that(
        engine
            .execute(Command::SetVisible {
                view,
                visible: false,
            })
            .is_ok(),
        "SetVisible is accepted",
    );
    check.that(
        engine
            .execute(Command::MuteAudio {
                view,
                muted: true,
            })
            .is_ok(),
        "MuteAudio is accepted",
    );

    // Engine-level interception: the host is on the policy blocklist.
    check.that(
        engine
            .execute(Command::Navigate {
                view,
                url: Url::parse("http://blocked.example/").unwrap(),
            })
            .is_ok(),
        "Navigate to a blocked host is accepted",
    );
    check.that(
        // The first real network request waits for Chromium's network service
        // to come up, which takes ~25 s in this sandbox; later requests are
        // immediate. The timeout covers that one-time startup, not the block.
        events.pump(&stream, Duration::from_secs(60), |e| {
            matches!(
                e,
                Event::LoadFailed {
                    reason: LoadFailure::BlockedByPolicy,
                    ..
                }
            )
        }),
        "blocked navigation reports LoadFailed::BlockedByPolicy",
    );

    check.that(
        engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::NativeChild {
                    width: 800,
                    height: 600,
                    scale: 1.0,
                },
                initial_url: None,
            })
            .is_err(),
        "NativeChild surface is refused",
    );

    check.that(
        engine
            .execute(Command::CloseView { view, force: false })
            .is_ok(),
        "CloseView is accepted",
    );
    check.that(
        events.pump(&stream, Duration::from_secs(20), |e| {
            matches!(e, Event::ViewClosed { .. })
        }),
        "ViewClosed arrives",
    );
    check.that(
        engine
            .execute(Command::Navigate {
                view,
                url: Url::parse("https://example.com/").unwrap(),
            })
            .is_err(),
        "commands for a closed view are rejected",
    );

    check.that(engine.shutdown().is_ok(), "shutdown returns Ok");
    check.that(
        events.pump(&stream, Duration::from_secs(10), |e| {
            matches!(e, Event::ShutdownComplete)
        }),
        "ShutdownComplete arrives",
    );

    events.dump();
    check.failures
}
