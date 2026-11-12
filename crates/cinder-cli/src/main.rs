//! `cinder` — a headless shell that exercises the orchestrator against the
//! scripted mock engine. It is the runnable proof that the adapter contract and
//! the tab/session model work end to end without any native code.

use std::sync::Arc;
use std::time::Duration;

use cinder_adapter::decision::{Decision, PermissionKind};
use cinder_adapter::policy::PolicySnapshot;
use cinder_adapter::sanitize::Url;
use cinder_adapter::Capabilities;
use cinder_adapter_mock::MockEngine;
use cinder_core::{Browser, Effect, NavigationOutcome};

fn main() {
    // CEF re-executes this binary for its subprocesses; they must exit before
    // any argument parsing so a helper process never runs a command.
    #[cfg(feature = "cef")]
    if let Some(code) = cinder_adapter_cef::Cef::execute_process() {
        std::process::exit(code);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let code = match command {
        "demo" => run_demo(),
        "browse" => run_browse(args.get(1).map(String::as_str)),
        "version" | "--version" | "-V" => {
            println!("cinder {} (adapter api 0.1)", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            2
        }
    };
    // `std::process::exit` skips destructors, so a piped stdout buffer would be
    // dropped and the report lost. Flush it explicitly.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn print_help() {
    println!("cinder {} — Cinder shell (mock engine)", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("  cinder demo      run a scripted session: tabs, policy, decisions, restore, shutdown");
    println!("  cinder browse <url>  load a real page in the CEF web engine");
    println!("  cinder version   print the version");
    println!("  cinder help      this text");
    println!();
    if cfg!(feature = "cef") {
        println!("This build links the CEF web engine; `browse` drives it.");
    } else {
        println!("This build has no web engine: `browse` needs --features cef.");
    }
}

fn run_demo() -> i32 {
    println!("Cinder demo — mock engine, no native code");
    println!("=========================================");

    let policy = Arc::new(
        PolicySnapshot::new(1)
            .with_blocked_host("doubleclick.net")
            .with_https_upgrade(true),
    );
    println!(
        "policy v{}: blocked={:?} https_upgrade={}",
        policy.version(),
        policy.blocked_hosts(),
        policy.https_upgrade()
    );

    let engine = MockEngine::builder()
        .capabilities(Capabilities::cef_baseline_expected())
        .request_permission(PermissionKind::Geolocation)
        .build();

    let mut browser = match Browser::new(
        Arc::new(engine.clone()),
        policy,
        "/tmp/cinder-demo-profile",
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to start orchestrator: {e}");
            return 1;
        }
    };

    let window = browser.open_window();
    println!("\nwindow {window} opened (profile {})", browser.profile());

    let targets = [
        "https://example.com/",
        "http://news.example.org/",
        "https://ads.doubleclick.net/tracker.js",
        "https://maps.example/",
    ];

    println!("\n-- navigation --");
    for target in targets {
        let url = match Url::parse(target) {
            Ok(u) => u,
            Err(e) => {
                println!("  {target:<40} rejected: {e}");
                continue;
            }
        };
        let tab = match browser.open_tab(window, None) {
            Ok(t) => t,
            Err(e) => {
                println!("  {target:<40} could not open tab: {e}");
                continue;
            }
        };
        match browser.navigate(tab, url) {
            Ok(NavigationOutcome::Started) => println!("  {target:<40} started in {tab}"),
            Ok(NavigationOutcome::Blocked) => {
                println!("  {target:<40} BLOCKED by policy");
                let _ = browser.close_tab(tab);
            }
            Ok(NavigationOutcome::UpgradedToHttps(u)) => {
                println!("  {target:<40} upgraded to {}", u.as_str())
            }
            Err(e) => println!("  {target:<40} error: {e}"),
        }
    }

    println!("\n-- effects --");
    for effect in browser.pump() {
        println!("  {}", describe(&effect));
    }

    println!("\n-- tabs --");
    println!("  {:<8} {:<38} {:<28} state", "id", "url", "title");
    for tab in browser.tabs() {
        let state = if tab.crashed {
            "crashed"
        } else if tab.loading {
            "loading"
        } else {
            "idle"
        };
        println!(
            "  {:<8} {:<38} {:<28} {}",
            tab.id.to_string(),
            truncate(tab.display_url(), 38),
            truncate(&tab.display_title(), 28),
            state
        );
    }

    println!("\n-- decisions --");
    let pending = browser.pending_decisions().len();
    println!("  {pending} request(s) waiting for the shell");
    if let Some(first) = browser.pending_decisions().first() {
        let id = first.id;
        let kind = first.kind.name();
        let answered = browser.answer_decision(id, Decision::AllowOnce);
        println!("  answered {id} ({kind}) with allow-once: {answered}");
    }
    println!("  history entries: {}", browser.history().len());

    println!("\n-- session snapshot --");
    let snapshot = browser.snapshot();
    let encoded = snapshot.encode();
    for line in encoded.lines() {
        println!("  {line}");
    }

    let restored_engine = MockEngine::new();
    let mut restored = match Browser::new(
        Arc::new(restored_engine),
        Arc::new(PolicySnapshot::new(2)),
        "/tmp/cinder-demo-profile",
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to start restore browser: {e}");
            return 1;
        }
    };
    let decoded = match cinder_core::SessionSnapshot::decode(&encoded) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("snapshot did not decode: {e}");
            return 1;
        }
    };
    match restored.restore(&decoded) {
        Ok(n) => println!("\n  restored {n} tab(s) into {} window(s)", decoded.windows.len()),
        Err(e) => {
            eprintln!("restore failed: {e}");
            return 1;
        }
    }
    let _ = restored.pump();
    for tab in restored.tabs() {
        println!("  restored {:<8} {}", tab.id.to_string(), tab.display_url());
    }

    println!("\n-- shutdown --");
    match browser.shutdown(Duration::from_millis(250)) {
        Ok(report) => println!(
            "  graceful: forced={} remaining={} complete={}",
            report.forced_closed, report.remaining, report.shutdown_complete
        ),
        Err(e) => {
            eprintln!("  shutdown failed: {e}");
            return 1;
        }
    }
    let _ = restored.shutdown(Duration::from_millis(250));

    println!("\n-- shutdown escalation (view refuses to close) --");
    let stubborn = MockEngine::builder().refuse_close(true).build();
    let mut stubborn_browser = match Browser::new(
        Arc::new(stubborn),
        Arc::new(PolicySnapshot::new(3)),
        "/tmp/cinder-demo-profile",
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to start escalation browser: {e}");
            return 1;
        }
    };
    let w = stubborn_browser.open_window();
    if let Err(e) = stubborn_browser.open_tab(w, Some(Url::parse("https://example.com/").unwrap())) {
        eprintln!("could not open tab: {e}");
        return 1;
    }
    let _ = stubborn_browser.pump();
    match stubborn_browser.shutdown(Duration::from_millis(50)) {
        Ok(report) => println!(
            "  escalated: forced={} remaining={} complete={}",
            report.forced_closed, report.remaining, report.shutdown_complete
        ),
        Err(e) => {
            eprintln!("  escalation failed: {e}");
            return 1;
        }
    }

    println!("\ndemo finished");
    0
}

#[cfg(feature = "cef")]
fn run_browse(target: Option<&str>) -> i32 {
    use cinder_adapter_cef::{CefEngine, CefEngineConfig};

    let Some(target) = target else {
        eprintln!("usage: cinder browse <url>");
        return 2;
    };
    let url = match Url::parse(target) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid url {target}: {e}");
            return 2;
        }
    };

    let staged = cinder_adapter_cef::prepare_runtime(&cinder_adapter_cef::cef_root());
    let config = CefEngineConfig {
        resources_dir: staged.clone(),
        locales_dir: staged.join("locales"),
        cache_dir: std::env::temp_dir().join("cinder-browse-cache"),
        extra_args: vec![
            "--disable-gpu".into(),
            "--disable-gpu-compositing".into(),
            "--disable-software-rasterizer".into(),
            "--disable-dev-shm-usage".into(),
            "--ozone-platform=headless".into(),
        ],
        capabilities: cinder_adapter_cef::measured_capabilities(),
    };
    println!("web engine : CEF {}", cinder_adapter_cef::Cef::version());
    println!("resources  : {}", config.resources_dir.display());

    let engine = match CefEngine::new(config) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("engine did not start: {e}");
            return 1;
        }
    };

    let policy = Arc::new(PolicySnapshot::new(1).with_https_upgrade(true));
    let mut browser = match Browser::new(engine, policy, "/tmp/cinder-browse-profile") {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to start orchestrator: {e}");
            return 1;
        }
    };

    let window = browser.open_window();
    let tab = match browser.open_tab(window, Some(url.clone())) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("could not open tab: {e}");
            return 1;
        }
    };
    println!("tab        : {tab} -> {}", url.as_str());

    // Pump until the load settles. The first network request in a fresh
    // profile waits for Chromium's network service, so allow a generous window.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut settled = false;
    while std::time::Instant::now() < deadline {
        for effect in browser.pump() {
            println!("  {}", describe(&effect));
            if matches!(
                effect,
                Effect::LoadFinished { .. } | Effect::LoadFailed { .. }
            ) {
                settled = true;
            }
        }
        if settled {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    match browser.tab(tab) {
        Some(t) => {
            println!("\ntitle      : {}", t.display_title());
            println!("url        : {}", t.display_url());
        }
        None => println!("\ntab is gone"),
    }
    if !settled {
        eprintln!("load did not settle within 60s");
    }

    match browser.shutdown(Duration::from_secs(5)) {
        Ok(report) => println!(
            "shutdown   : forced={} remaining={} complete={}",
            report.forced_closed, report.remaining, report.shutdown_complete
        ),
        Err(e) => {
            eprintln!("shutdown failed: {e}");
            return 1;
        }
    }
    if settled {
        0
    } else {
        1
    }
}

#[cfg(not(feature = "cef"))]
fn run_browse(_target: Option<&str>) -> i32 {
    eprintln!("this build has no web engine; rebuild with --features cef");
    2
}

fn describe(effect: &Effect) -> String {    match effect {
        Effect::TabOpened(tab) => format!("{tab} opened"),
        Effect::TabClosed(tab) => format!("{tab} closed"),
        Effect::CloseRequested(tab) => format!("{tab} page requested close"),
        Effect::TitleChanged { tab, title } => format!("{tab} title -> {title}"),
        Effect::UrlChanged { tab, url } => format!("{tab} url -> {url}"),
        Effect::LoadStarted { tab } => format!("{tab} load started"),
        Effect::LoadFinished { tab } => format!("{tab} load finished"),
        Effect::LoadFailed { tab, reason } => format!("{tab} load failed ({reason:?})"),
        Effect::RendererCrashed { tab } => format!("{tab} renderer crashed"),
        Effect::ViewUnresponsive { tab } => format!("{tab} unresponsive"),
        Effect::NavigationBlocked { tab, url } => format!("{tab} blocked {url}"),
        Effect::PermissionPrompt { tab, kind, origin } => {
            format!("{tab} asks for {kind} on {origin}")
        }
        Effect::DecisionRequired { tab, kind } => format!("{tab} needs a {kind} decision"),
        Effect::EngineFault { message } => format!("engine fault: {message}"),
        Effect::ShutdownComplete => "engine shutdown complete".to_string(),
    }
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let kept: String = input.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}
