//! CEF spike: prove that the native boundary works end to end.
//!
//! Run with `cargo run -p cinder-adapter-cef --bin cef-spike`. It initializes
//! CEF, creates one windowless browser, loads an inline page, waits for a
//! painted frame, writes it as a PPM, and prints what it observed. Every line of
//! the capability table is derived from that run, not from documentation.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cinder_adapter_cef::{Cef, STATE_CLOSED, STATE_CREATED};

const PAGE: &str = "data:text/html,<html><body style='margin:0;background:%23101828;color:%23f0f4ff;font-family:sans-serif'><h1 style='padding:24px'>Cinder &mdash; CEF spike</h1><p style='padding:0 24px'>Rendered by Chromium in a windowless view.</p><div style='margin:24px;width:320px;height:80px;background:%2334d399'></div></body></html>";

fn main() {
    // CEF re-executes this binary for its subprocesses; they must exit at once.
    if let Some(code) = Cef::execute_process() {
        std::process::exit(code);
    }

    let root = std::env::var("CINDER_CEF_ROOT").unwrap_or_else(|_| {
        format!("{}/cef", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
    });
    let root = PathBuf::from(root);
    // Chromium resolves its resource files relative to the executable, and a
    // path containing spaces has already caused trouble here, so prefer the
    // executable's own directory when the resource files are present there.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let resources = if exe_dir.join("icudtl.dat").exists()
        && exe_dir.join("v8_context_snapshot.bin").exists()
    {
        exe_dir.clone()
    } else {
        root.join("Resources")
    };
    let locales = resources.join("locales");
    let cache = match std::env::var("CINDER_CEF_CACHE") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::temp_dir().join("cinder-cef-cache"),
    };
    let _ = std::fs::create_dir_all(&cache);
    // Chromium's process singleton needs an AF_UNIX socket, which some sandboxes
    // deny. Omitting the cache path avoids creating it.
    let cache_arg = if std::env::var("CINDER_CEF_NO_CACHE").is_ok() {
        String::new()
    } else {
        cache.to_string_lossy().into_owned()
    };

    println!("cef root   : {}", root.display());
    println!("resources  : {} (exists: {})", resources.display(), resources.exists());
    println!("locales    : {} (exists: {})", locales.display(), locales.exists());

    let cef = match Cef::init_with_args(
        &resources,
        &locales,
        std::path::Path::new(&cache_arg),
        &[
            "--disable-gpu".to_string(),
            "--disable-dev-shm-usage".to_string(),
            "--no-zygote".to_string(),
            "--single-process".to_string(),
            "--ozone-platform=headless".to_string(),
            format!("--user-data-dir={}", cache.display()),
        ],
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("FAIL init: {e}");
            std::process::exit(1);
        }
    };
    println!("version    : {}", Cef::version());

    let (w, h) = (800u32, 600u32);
    if let Err(e) = cef.create_browser(PAGE, w, h) {
        eprintln!("FAIL create_browser: {e}");
        cef.shutdown();
        std::process::exit(1);
    }
    println!("browser    : created {w}x{h} windowless");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut first_paint: Option<Duration> = None;
    let mut load_at: Option<Duration> = None;
    let started = Instant::now();

    while Instant::now() < deadline {
        cef.pump(50);
        let state = cef.state();
        if first_paint.is_none() && state & cinder_adapter_cef::STATE_PAINTED != 0 {
            first_paint = Some(started.elapsed());
        }
        if load_at.is_none() && state & cinder_adapter_cef::STATE_LOADED != 0 {
            load_at = Some(started.elapsed());
        }
        if first_paint.is_some() && load_at.is_some() {
            break;
        }
    }

    // Give the compositor a moment to settle on the final frame.
    cef.pump(300);

    let state = cef.state();
    let frame = cef.frame();
    let title = cef.title();
    let url = cef.url();

    println!("\n-- observations --");
    println!("state      : created={} loaded={} painted={} closed={}",
        state & STATE_CREATED != 0,
        state & cinder_adapter_cef::STATE_LOADED != 0,
        state & cinder_adapter_cef::STATE_PAINTED != 0,
        state & STATE_CLOSED != 0);
    println!("first paint: {}", first_paint.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "never".into()));
    println!("load done  : {}", load_at.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "never".into()));
    println!("title      : {title:?}");
    println!("url        : {}", if url.len() > 80 { &url[..80] } else { &url });

    let mut ok = true;
    match &frame {
        Some(f) => {
            let non_white = f.non_white_pixels();
            let colors = f.distinct_colors(4096);
            println!(
                "frame      : {}x{} ({} bytes), non-white pixels={}, distinct colors={}",
                f.width,
                f.height,
                f.bgra.len(),
                non_white,
                colors
            );
            let out = std::env::temp_dir().join("cinder-cef-spike.ppm");
            match f.write_ppm(&out) {
                Ok(()) => println!("ppm        : {} ({} bytes)", out.display(), std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0)),
                Err(e) => {
                    println!("ppm        : FAILED {e}");
                    ok = false;
                }
            }
            if f.width != w || f.height != h {
                println!("WARN       : frame size differs from requested size");
            }
            if non_white == 0 {
                println!("FAIL       : frame is entirely white");
                ok = false;
            }
        }
        None => {
            println!("frame      : none");
            ok = false;
        }
    }

    println!("\n-- capability table (measured) --");
    println!("{:<28} {}", "offscreen_rendering", yes(frame.is_some()));
    println!("{:<28} {}", "load_events", yes(load_at.is_some()));
    println!("{:<28} {}", "title_reporting", yes(!title.is_empty()));
    println!("{:<28} {}", "url_reporting", yes(!url.is_empty()));
    for name in [
        "incognito_contexts",
        "download_pause",
        "print_to_pdf",
        "find_in_page",
        "devtools",
        "request_interception",
    ] {
        println!("{name:<28} not exercised");
    }

    cef.shutdown();
    println!("\nshutdown   : clean");
    println!("result     : {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}

fn yes(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
