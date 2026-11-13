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

#[cfg(feature = "window")]
mod update;

/// The version this binary was published as: the release tag when the build
/// baked one in, otherwise the crate version.
fn version_string() -> &'static str {
    match option_env!("CINDER_RELEASE_TAG") {
        Some(tag) if !tag.is_empty() => tag.trim_start_matches('v'),
        _ => env!("CARGO_PKG_VERSION"),
    }
}

fn main() {
    // A crash inside CEF leaves no output at all when stdout is a pipe, so the
    // selftest writes its first line to stderr before anything else runs.
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: main entered, argv={:?}", std::env::args().collect::<Vec<_>>());
    }
    // CEF re-executes this binary for its subprocesses; they must exit before
    // any argument parsing so a helper process never runs a command.
    #[cfg(feature = "cef")]
    if let Some(code) = cinder_adapter_cef::Cef::execute_process() {
        if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
            eprintln!("window trace: subprocess exit {code}");
        }
        std::process::exit(code);
    }

    // Diagnostic hook for CI: try one CEF initialization variant and report it.
    // A crash inside libcef takes the whole process down, so the caller reads
    // the exit code and every variant runs in its own process.
    #[cfg(feature = "cef")]
    if let Some(probe) = std::env::var_os("CINDER_CEF_PROBE") {
        let code = run_probe(&probe.to_string_lossy());
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    // No arguments means the desktop entry was clicked: open the window rather
    // than printing help into a terminal nobody can see.
    let command = args.first().map(String::as_str).unwrap_or("open");

    let code = match command {
        "demo" => run_demo(),
        "browse" => run_browse(args.get(1).map(String::as_str)),
        "open" => run_open(args.get(1).map(String::as_str)),
        "update" => run_update(),
        other if looks_like_url(other) => run_open(Some(other)),
        "version" | "--version" | "-V" => {
            println!("cinder {} (adapter api 0.1)", version_string());
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

/// Try one CEF initialization variant, selected by `CINDER_CEF_PROBE`.
///
/// The window selftest runs each variant in its own process: a crash inside
/// libcef kills the process, so the exit code is the result. This exists to
/// tell a thread-affinity problem apart from a settings or GPU problem.
#[cfg(feature = "cef")]
fn run_probe(name: &str) -> i32 {
    use cinder_adapter_cef::{CefEngine, CefEngineConfig};

    let staged = cinder_adapter_cef::prepare_runtime(&cinder_adapter_cef::cef_root());
    let cache = std::env::temp_dir().join("cinder-probe-cache");
    let mut config = CefEngineConfig {
        resources_dir: staged.clone(),
        locales_dir: staged.join("locales"),
        cache_dir: cache,
        extra_args: window_args(),
        windowless: false,
        main_thread: cinder_adapter_cef::default_main_thread(),
        capabilities: cinder_adapter_cef::measured_capabilities(),
    };
    match name {
        // `main` initializes on this thread, `thread` on a dedicated one; the
        // rest vary windowless rendering, single-process and the GPU process.
        "main" | "main-windowless" | "thread" | "thread-windowless" | "single" | "inproc" => {
            config.windowless = name.ends_with("windowless");
            config.main_thread = name.starts_with("main");
            match name {
                "single" => config.extra_args.push("--single-process".into()),
                "inproc" => config.extra_args.push("--in-process-gpu".into()),
                _ => {}
            }
            match CefEngine::new(config) {
                Ok(_) => {
                    println!("probe {name}: init ok");
                    0
                }
                Err(e) => {
                    println!("probe {name}: init failed: {e}");
                    1
                }
            }
        }
        other => {
            println!("probe {other}: unknown variant");
            2
        }
    }
}

fn print_help() {
    println!("cinder {} — Cinder shell (mock engine)", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("  cinder <url>     open the browser window on a page");
    println!("  cinder open <url>  same as above");
    println!("  cinder demo      run a scripted session: tabs, policy, decisions, restore, shutdown");
    println!("  cinder browse <url>  load a real page headlessly and report the result");
    println!("  cinder update    check the published releases for a newer version");
    println!("  cinder version   print the version");
    println!("  cinder help      this text");
    println!();
    if cfg!(feature = "window") {
        println!("This build opens a window; `browse` stays headless.");
    } else if cfg!(feature = "cef") {
        println!("This build links the CEF web engine but has no window layer.");
    } else {
        println!("This build has no web engine: `browse` needs --features cef.");
    }
}

/// A bare argument that names a page rather than a subcommand.
fn looks_like_url(arg: &str) -> bool {
    if arg.contains("://") || arg.starts_with("www.") || arg.contains('.') {
        return true;
    }
    // Scheme-only URLs such as `about:blank` carry no dot and no `//`, so they
    // would otherwise be rejected as unknown commands.
    match arg.split_once(':') {
        Some((scheme, _)) => {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
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
#[cfg(feature = "window")]
fn run_update() -> i32 {
    let install = std::env::args().any(|a| a == "--install");
    let current = update::current();
    println!("installed  : {current}");
    match update::check(&current) {
        Ok(Some(release)) => {
            println!("available  : {}", release.version);
            println!("page       : {}", release.page);
            if let Some(package) = release.package.as_ref() {
                println!("package    : {package}");
            }
            if !install {
                println!("run `cinder update --install` to install it");
                return 0;
            }
            match update::install(&release) {
                Ok(()) => {
                    println!("installed  : {}", release.version);
                    0
                }
                Err(e) => {
                    eprintln!("install failed: {e}");
                    1
                }
            }
        }
        Ok(None) => {
            println!("up to date");
            0
        }
        Err(e) => {
            eprintln!("check failed: {e}");
            1
        }
    }
}

#[cfg(not(feature = "window"))]
fn run_update() -> i32 {
    eprintln!("this build has no updater; rebuild with --features window");
    2
}

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
        windowless: true,
        main_thread: cinder_adapter_cef::default_main_thread(),
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

#[cfg(feature = "window")]
/// Chromium switches for the windowed path.
fn window_args() -> Vec<String> {
    // The shell is an X11 window (XWayland under a Wayland session) and CEF
    // embeds an X11 child in it, so Chromium must not pick the Wayland
    // backend. Windows and macOS have no ozone backends and reject the switch.
    #[cfg(target_os = "linux")]
    let mut args: Vec<String> = vec![
        "--ozone-platform=x11".into(),
        // No /dev/shm in some containers.
        "--disable-dev-shm-usage".into(),
    ];
    #[cfg(not(target_os = "linux"))]
    let mut args: Vec<String> = Vec::new();
    // No GPU on CI runners.
    args.push("--disable-gpu".into());
    // A headless macOS runner has no window server session for the GPU
    // process, which then aborts the browser with "GPU process isn't usable".
    #[cfg(target_os = "macos")]
    args.push("--disable-gpu-sandbox".into());
    args
}

fn run_open(target: Option<&str>) -> i32 {
    use std::cell::RefCell;
    use std::rc::Rc;

    use cinder_adapter_cef::{CefEngine, CefEngineConfig};

    let target = target.unwrap_or("about:blank");
    let url = match Url::parse(target) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid url {target}: {e}");
            return 2;
        }
    };

    let staged = cinder_adapter_cef::prepare_runtime(&cinder_adapter_cef::cef_root());
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: runtime staged at {}", staged.display());
    }
    let config = CefEngineConfig {
        resources_dir: staged.clone(),
        locales_dir: staged.join("locales"),
        cache_dir: std::env::temp_dir().join("cinder-window-cache"),
        // Windowless rendering needs software compositing to produce OnPaint
        // frames; the GPU path paints into a surface nobody can read. CEF's own
        // display connection stays headless — the visible window belongs to
        // winit, not to Chromium. The software rasterizer must stay enabled:
        // disabling it leaves every frame black.
        extra_args: window_args(),
        windowless: false,
        main_thread: cinder_adapter_cef::default_main_thread(),
        capabilities: cinder_adapter_cef::measured_capabilities(),
    };

    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: engine starting");
    }
    let engine = match CefEngine::new(config) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("engine did not start: {e}");
            return 1;
        }
    };

    // The window may abort inside CEF; flush now so the lines above survive.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: engine started");
    }

    let policy = Arc::new(PolicySnapshot::new(1).with_https_upgrade(true));
    let profile = std::env::temp_dir().join("cinder-window-profile");
    let profile = profile.to_string_lossy().into_owned();
    let browser = match Browser::new(engine.clone(), policy, &profile) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to start orchestrator: {e}");
            return 1;
        }
    };

    // The window loop owns the main thread and the orchestrator stays on it, so
    // a plain `Rc<RefCell<_>>` is enough; no locking is needed. The page is
    // created from inside the loop because its child window needs the shell's
    // native handle, which only exists once the window does.
    let shared = Rc::new(RefCell::new(browser));
    let pump = Rc::clone(&shared);
    let opener = Rc::clone(&shared);
    let probe = Arc::clone(&engine);
    let debug = std::env::var_os("CINDER_WINDOW_SELFTEST").is_some();
    let mut ticks = 0u32;
    // The update check runs on its own thread: the window must never wait for
    // the network, and the answer only changes the status strip.
    let status = Arc::new(std::sync::Mutex::new(String::new()));
    let (click_tx, click_rx) = std::sync::mpsc::channel();
    let (found_tx, found_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let installed = update::current();
    println!("version    : {installed}");
    if std::env::var_os("CINDER_NO_UPDATE_CHECK").is_none() {
        let installed = installed.clone();
        std::thread::spawn(move || {
            let _ = found_tx.send(update::check(&installed));
        });
    }
    let ui = cinder_window::Ui {
        status: Arc::clone(&status),
        clicked: Some(click_tx),
    };
    let mut pending: Option<update::Release> = None;
    let mut installing = false;
    let mut notice = String::new();
    let options = cinder_window::WindowOptions {
        title: "Cinder".to_string(),
        ..Default::default()
    };
    let result = cinder_window::run(
        engine.clone(),
        options,
        ui,
        move |parent| {
            use cinder_adapter::command::Surface;
            let mut browser = opener.borrow_mut();
            browser.set_surface(Surface::NativeChild {
                parent: parent.handle,
                top: parent.top,
                width: parent.width,
                height: parent.height,
                scale: parent.scale,
            });
            let window = browser.open_window();
            match browser.open_tab(window, Some(url.clone())) {
                Ok(tab) => {
                    println!("web engine : CEF {}", cinder_adapter_cef::Cef::version());
                    println!("tab        : {tab} -> {}", url.as_str());
                    browser.tab(tab).map(|t| t.view)
                }
                Err(e) => {
                    eprintln!("could not open tab: {e}");
                    None
                }
            }
        },
        move || {
        let mut browser = pump.borrow_mut();
        for effect in browser.pump() {
            match effect {
                Effect::TitleChanged { title, .. } => println!("  title -> {title}"),
                Effect::LoadFinished { .. } => println!("  load finished"),
                Effect::LoadFailed { reason, .. } => println!("  load failed ({reason:?})"),
                Effect::EngineFault { message } => eprintln!("  engine fault: {message}"),
                other => {
                    if debug {
                        println!("  {}", describe(&other));
                    }
                }
            }
        }
        ticks += 1;
        if debug && ticks % 60 == 0 {
            let view = browser.tabs().next().map(|t| t.view);
            let frame = view.and_then(|v| probe.copy_frame(v));
            println!(
                "  tick {ticks}: frame={:?}",
                frame.map(|f| (f.width, f.height))
            );
        }

        match found_rx.try_recv() {
            Ok(Ok(Some(release))) => {
                println!(
                    "update     : {} available ({})",
                    release.version, release.page
                );
                notice = format!(
                    "Cinder {} is available - click here to install",
                    release.version
                );
                pending = Some(release);
            }
            Ok(Err(e)) if debug => {
                eprintln!("update     : {e}");
            }
            _ => {}
        }
        if click_rx.try_recv().is_ok() && !installing {
            if let Some(release) = pending.clone() {
                installing = true;
                notice = format!("Installing Cinder {}...", release.version);
                let done = done_tx.clone();
                std::thread::spawn(move || {
                    let _ = done.send(update::install(&release));
                });
            }
        }
        if let Ok(result) = done_rx.try_recv() {
            installing = false;
            match result {
                Ok(()) => {
                    notice = "Update installed - restart Cinder".to_string();
                    pending = None;
                }
                Err(e) => {
                    eprintln!("update     : {e}");
                    notice = format!("Update failed: {e}");
                }
            }
        }
        if let Ok(mut slot) = status.lock() {
            if *slot != notice {
                *slot = notice.clone();
            }
        }
        },
    );

    let mut browser = shared.borrow_mut();
    let shutdown = browser.shutdown(Duration::from_secs(5));
    match (result, shutdown) {
        (Err(e), _) => {
            eprintln!("window failed: {e}");
            1
        }
        (Ok(()), Ok(report)) => {
            println!(
                "shutdown   : forced={} remaining={} complete={}",
                report.forced_closed, report.remaining, report.shutdown_complete
            );
            0
        }
        (Ok(()), Err(e)) => {
            eprintln!("shutdown failed: {e}");
            1
        }
    }
}

#[cfg(all(feature = "cef", not(feature = "window")))]
fn run_open(_target: Option<&str>) -> i32 {
    eprintln!("this build has no window layer; rebuild with --features window");
    2
}

#[cfg(not(feature = "cef"))]
fn run_open(_target: Option<&str>) -> i32 {
    eprintln!("this build has no web engine; rebuild with --features window");
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

#[cfg(test)]
mod tests {
    use super::looks_like_url;

    #[test]
    fn recognises_urls_and_schemes() {
        for url in [
            "https://example.com",
            "http://127.0.0.1:8099/",
            "www.example.com",
            "example.com",
            "about:blank",
            "file:///tmp/page.html",
        ] {
            assert!(looks_like_url(url), "{url} should be treated as a URL");
        }
    }

    #[test]
    fn rejects_commands_and_plain_words() {
        for arg in ["demo", "help", "version", "update", "browse", "open"] {
            assert!(!looks_like_url(arg), "{arg} should not be a URL");
        }
        assert!(!looks_like_url(":blank"));
        assert!(!looks_like_url("1nvalid:scheme"));
    }
}
