//! The window layer: an OS window whose page area is a native child window
//! owned by CEF.
//!
//! CEF runs in windowed mode here. It creates a child window inside the shell's
//! window and paints into it directly, so the shell never copies pixels and
//! never replays input: the compositor and the X server deliver both. The shell
//! owns only the toolbar, the address field and the status strip, which it draws
//! into the part of the window the child does not cover.
//!
//! The event loop stays on the main thread, which is what winit requires; the
//! engine keeps its own thread and is reached only through channels.

mod chrome;
mod font;

pub use chrome::TabChip;

use std::num::NonZeroU32;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinder_adapter::command::Command;
use cinder_adapter::engine::Engine;
use cinder_adapter::ids::ViewId;
use cinder_adapter::sanitize::Url;
use cinder_adapter_cef::CefEngine;
use chrome::{Chrome, Edit, Hit};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

/// How often the loop wakes to pump the engine and repaint.
const TICK: Duration = Duration::from_millis(16);

/// Progress notes for `CINDER_WINDOW_SELFTEST`. They go to stderr, which is
/// unbuffered, so a crash still leaves a trail of how far the window got.
fn trace(message: &str) {
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: {message}");
        if let Ok(mut slot) = STAGE.lock() {
            *slot = Some(message.to_string());
        }
    }
}

/// The last step reached, so a watchdog can name where a hang is stuck.
static STAGE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// How long the selftest may go without finishing before it reports the step it
/// is stuck in and exits. A hang inside winit or CEF never returns, so the
/// outer shell bound would otherwise kill the process with no explanation.
const SELFTEST_STALL: Duration = Duration::from_secs(45);

/// Why the window could not run.
#[derive(Debug)]
pub enum WindowError {
    /// The platform refused an event loop, usually because there is no display.
    EventLoop(String),
    /// The window or its drawing surface could not be created.
    Surface(String),
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EventLoop(m) => write!(f, "event loop: {m}"),
            Self::Surface(m) => write!(f, "surface: {m}"),
        }
    }
}

impl std::error::Error for WindowError {}

/// Window size and title for a new shell window.
#[derive(Debug, Clone)]
pub struct WindowOptions {
    pub title: String,
    pub width: u32,
    pub height: u32,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "Cinder".to_string(),
            width: 1280,
            height: 800,
        }
    }
}

/// Hooks the shell uses to talk to the caller's status strip.
///
/// The caller owns the update check and runs it on its own thread; it writes
/// the strip's text here and learns about clicks through the channel.
#[derive(Debug, Clone, Default)]
pub struct Ui {
    /// Text shown in the status strip.
    pub status: Arc<Mutex<String>>,
    /// Fired when the user clicks the status strip.
    pub clicked: Option<Sender<()>>,
}

/// The shell's native window, handed to the engine so it can embed the page.
#[derive(Debug, Clone, Copy)]
pub struct NativeParent {
    /// Platform handle: an X11 window id on Linux, an `HWND` on Windows, an
    /// `NSView` pointer on macOS.
    pub handle: u64,
    /// Size of the area the page occupies, below the toolbar and above the
    /// status strip.
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

/// Build the shell's event loop.
///
/// macOS delivers `resumed` from `applicationDidFinishLaunching`, which AppKit
/// only sends to the delegate installed before the app starts running. CEF
/// touches `NSApplication` while it initializes, so the loop has to exist
/// first: otherwise the delegate is installed after the callback already
/// happened and the shell waits forever for a `resumed` that never comes.
pub fn event_loop() -> Result<EventLoop<()>, WindowError> {
    let mut builder = EventLoop::builder();
    // CEF embeds an X11 child window, so the shell must be an X11 window even
    // when the session is Wayland; XWayland provides the display.
    #[cfg(target_os = "linux")]
    builder.with_x11();
    trace("event loop building");
    let event_loop = builder
        .build()
        .map_err(|e| WindowError::EventLoop(e.to_string()))?;
    trace("event loop built");
    Ok(event_loop)
}

/// Run the window until the user closes it.
///
/// `open` is called once the window exists, with the handle the engine needs to
/// create the page's child window; it returns the view to drive. `tick` is
/// called once per frame so the caller can pump the orchestrator.
pub fn run(
    engine: Arc<CefEngine>,
    options: WindowOptions,
    ui: Ui,
    open: impl FnOnce(NativeParent) -> Option<ViewId> + 'static,
    tick: impl FnMut() + 'static,
) -> Result<(), WindowError> {
    let event_loop = event_loop()?;
    run_on(event_loop, engine, options, ui, open, tick)
}

/// Tab operations the shell delegates to the caller, which owns the browser.
///
/// The window layer never sees `TabId`; it passes back the opaque key the host
/// returned when the tab was opened.
pub struct TabHost {
    /// Open a tab and create its view. The shell switches to it.
    pub open: Box<dyn FnMut(NativeParent) -> Option<(u64, ViewId)>>,
    /// Make an existing tab current and return its view.
    pub activate: Box<dyn FnMut(u64) -> Option<ViewId>>,
    /// Close a tab and release its view.
    pub close: Box<dyn FnMut(u64)>,
    /// Current tabs, left to right, with the active one flagged.
    pub list: Box<dyn Fn() -> Vec<(u64, TabChip)>>,
}

impl std::fmt::Debug for TabHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TabHost")
    }
}

impl TabHost {
    /// A host with no tabs: the strip is not drawn and the shortcuts do
    /// nothing, which is what the single-page selftest wants.
    pub fn disabled() -> Self {
        Self {
            open: Box::new(|_| None),
            activate: Box::new(|_| None),
            close: Box::new(|_| {}),
            list: Box::new(Vec::new),
        }
    }
}

/// Like [`run`], but with a tab strip driven by `tabs`.
///
/// `open` creates the first tab and returns its key and view; the host's
/// `list` supplies the strip contents from then on.
pub fn run_with_tabs(
    engine: Arc<CefEngine>,
    options: WindowOptions,
    ui: Ui,
    open: impl FnOnce(NativeParent) -> Option<(u64, ViewId)> + 'static,
    tick: impl FnMut() + 'static,
    tabs: TabHost,
) -> Result<(), WindowError> {
    let event_loop = event_loop()?;
    run_on_with_tabs(event_loop, engine, options, ui, open, tick, tabs)
}

/// Run the window on an event loop the caller already built.
pub fn run_on(
    event_loop: EventLoop<()>,
    engine: Arc<CefEngine>,
    options: WindowOptions,
    ui: Ui,
    open: impl FnOnce(NativeParent) -> Option<ViewId> + 'static,
    tick: impl FnMut() + 'static,
) -> Result<(), WindowError> {
    run_on_with_tabs(
        event_loop,
        engine,
        options,
        ui,
        move |parent| open(parent).map(|view| (0, view)),
        tick,
        TabHost::disabled(),
    )
}

/// Run the window on an existing loop with a tab strip.
pub fn run_on_with_tabs(
    event_loop: EventLoop<()>,
    engine: Arc<CefEngine>,
    options: WindowOptions,
    ui: Ui,
    open: impl FnOnce(NativeParent) -> Option<(u64, ViewId)> + 'static,
    tick: impl FnMut() + 'static,
    tabs: TabHost,
) -> Result<(), WindowError> {
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    let mut app = Shell {
        engine,
        view: None,
        views: std::collections::HashMap::new(),
        keys: Vec::new(),
        active: None,
        tabs,
        handle: None,
        quit: false,
        open: Some(Box::new(open)),
        options,
        ui,
        tick: Box::new(tick),
        window: None,
        surface: None,
        context: None,
        chrome: Chrome::new(1.0),
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        last_size: (0, 0),
        ticks: 0,
        presented: 0,
        last_reason: "none",
        selftest: std::env::var("CINDER_WINDOW_SELFTEST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    trace("event loop running");
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        std::thread::spawn(|| {
            std::thread::sleep(SELFTEST_STALL);
            let stage = STAGE
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .unwrap_or_else(|| "start".to_string());
            eprintln!("window selftest: stalled at {stage}");
            std::process::exit(70);
        });
    }
    event_loop
        .run_app(&mut app)
        .map_err(|e| WindowError::EventLoop(e.to_string()))
}

/// The first page's opener, kept until the window handle exists.
type Opener = Box<dyn FnOnce(NativeParent) -> Option<(u64, ViewId)>>;

struct Shell {
    engine: Arc<CefEngine>,
    /// Set once the window exists and `open` has created the page.
    view: Option<ViewId>,
    /// Every open tab's view, keyed by the host's opaque tab key.
    views: std::collections::HashMap<u64, ViewId>,
    /// Tab keys in strip order.
    keys: Vec<u64>,
    /// The key of the tab whose view is showing.
    active: Option<u64>,
    /// Tab operations, delegated to the caller.
    tabs: TabHost,
    /// The window's platform handle, kept so new tabs can be parented.
    handle: Option<u64>,
    /// Set when the last tab closed, so the loop exits after this frame.
    quit: bool,
    open: Option<Opener>,
    options: WindowOptions,
    ui: Ui,
    tick: Box<dyn FnMut()>,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    /// Toolbar, address field and status strip.
    chrome: Chrome,
    modifiers: ModifiersState,
    cursor: (f64, f64),
    last_size: (u32, u32),
    ticks: u32,
    presented: u32,
    /// Why the last redraw did or did not present, for the selftest report.
    last_reason: &'static str,
    /// Set by `CINDER_WINDOW_SELFTEST`: run for a bounded number of frames and
    /// report what was presented, so the window path can be checked without a
    /// human looking at the screen. Zero means run until closed.
    selftest: u32,
}

impl Shell {
    fn resize_view(&mut self, width: u32, height: u32, scale: f32) {
        let Some(view) = self.view else {
            return;
        };
        if width == 0 || height == 0 || (width, height) == self.last_size {
            return;
        }
        self.last_size = (width, height);
        self.chrome.scale = scale;
        let view_height = height
            .saturating_sub(self.chrome.chrome_px())
            .saturating_sub(self.chrome.status_px())
            .max(1);
        let _ = self.engine.execute(Command::Resize {
            view,
            width,
            height: view_height,
            scale,
        });
    }

    fn size(&self) -> (u32, u32) {
        self.window
            .as_ref()
            .map(|w| {
                let size = w.inner_size();
                (size.width, size.height)
            })
            .unwrap_or((0, 0))
    }

    /// What the pointer is over, chrome first.
    fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        let (width, height) = self.size();
        self.chrome
            .hit(x, y, width)
            .or_else(|| self.chrome.over_status(y, height).then_some(Hit::Status))
    }

    fn activate(&mut self, hit: Hit) {
        match hit {
            Hit::Back => self.back(),
            Hit::Forward => self.forward(),
            Hit::Reload => self.reload(),
            Hit::Url => {
                self.chrome.begin_edit();
                if let Some(view) = self.view {
                    let _ = self.engine.execute(Command::SetFocus {
                        view,
                        focused: false,
                    });
                }
            }
            Hit::Status => {
                if let Some(clicked) = self.ui.clicked.as_ref() {
                    let _ = clicked.send(());
                }
            }
            Hit::Tab(index) => {
                if let Some(key) = self.keys.get(index).copied() {
                    self.switch_to(key);
                }
            }
            Hit::TabClose(index) => {
                if let Some(key) = self.keys.get(index).copied() {
                    self.close_tab(key);
                }
            }
            Hit::NewTab => self.new_tab(),
        }
    }

    /// Show the tab's view and hide the one that was showing.
    fn switch_to(&mut self, key: u64) {
        if self.active == Some(key) {
            return;
        }
        let view = match self.views.get(&key).copied() {
            Some(view) => view,
            None => match (self.tabs.activate)(key) {
                Some(view) => {
                    self.views.insert(key, view);
                    view
                }
                None => return,
            },
        };
        if let Some(old) = self.view {
            let _ = self.engine.execute(Command::SetVisible {
                view: old,
                visible: false,
            });
        }
        self.view = Some(view);
        self.active = Some(key);
        let _ = self.engine.execute(Command::SetVisible {
            view,
            visible: true,
        });
        let _ = self.engine.execute(Command::SetFocus {
            view,
            focused: true,
        });
        // The new view was created at the size the window had then; make sure
        // it matches the current one before it paints.
        let (width, height) = self.size();
        self.last_size = (0, 0);
        self.resize_view(width, height, self.chrome.scale);
    }

    /// Open a tab through the host and switch to it.
    fn new_tab(&mut self) {
        let Some(parent) = self.parent() else {
            return;
        };
        let Some((key, view)) = (self.tabs.open)(parent) else {
            return;
        };
        self.views.insert(key, view);
        self.keys.push(key);
        self.switch_to(key);
    }

    /// Close a tab, then fall back to a neighbour or close the window.
    fn close_tab(&mut self, key: u64) {
        let index = self.keys.iter().position(|k| *k == key);
        (self.tabs.close)(key);
        if let Some(view) = self.views.remove(&key) {
            let _ = self.engine.execute(Command::CloseView { view, force: false });
        }
        self.keys.retain(|k| *k != key);
        if self.active != Some(key) {
            return;
        }
        self.view = None;
        self.active = None;
        let next = index
            .and_then(|i| self.keys.get(i).copied().or_else(|| self.keys.last().copied()));
        match next {
            Some(key) => self.switch_to(key),
            None => {
                // Last tab closed: the window has nothing left to show.
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
                self.quit = true;
            }
        }
    }

    /// Move to the next or previous tab, wrapping at the ends.
    fn cycle_tab(&mut self, delta: isize) {
        if self.keys.len() < 2 {
            return;
        }
        let Some(active) = self.active else {
            return;
        };
        let Some(index) = self.keys.iter().position(|k| *k == active) else {
            return;
        };
        let len = self.keys.len() as isize;
        let next = (index as isize + delta).rem_euclid(len) as usize;
        if let Some(key) = self.keys.get(next).copied() {
            self.switch_to(key);
        }
    }

    /// The parent handle and geometry for a new page, if the window exists.
    fn parent(&self) -> Option<NativeParent> {
        self.window.as_ref()?;
        let handle = self.handle?;
        let (width, height) = self.size();
        let view_height = height
            .saturating_sub(self.chrome.chrome_px())
            .saturating_sub(self.chrome.status_px())
            .max(1);
        Some(NativeParent {
            handle,
            top: self.chrome.chrome_px(),
            width: width.max(1),
            height: view_height,
            scale: self.chrome.scale,
        })
    }

    fn back(&mut self) {
        if let Some(view) = self.view {
            let _ = self.engine.execute(Command::GoBack { view });
        }
    }

    fn forward(&mut self) {
        if let Some(view) = self.view {
            let _ = self.engine.execute(Command::GoForward { view });
        }
    }

    fn reload(&mut self) {
        if let Some(view) = self.view {
            let _ = self.engine.execute(Command::Reload {
                view,
                bypass_cache: false,
            });
        }
    }

    /// Navigate to whatever the address field holds.
    fn navigate(&mut self, text: &str) {
        let Some(url) = normalize(text) else {
            return;
        };
        self.chrome.cancel_edit();
        let Some(view) = self.view else {
            return;
        };
        let _ = self.engine.execute(Command::SetFocus {
            view,
            focused: true,
        });
        let _ = self.engine.execute(Command::Navigate { view, url });
    }

    /// Repaint the chrome. The page area belongs to CEF's child window, so this
    /// only has to cover the toolbar and the status strip.
    fn present(&mut self) -> &'static str {
        let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref()) else {
            return "no surface";
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return "zero size";
        };
        if surface.resize(w, h).is_err() {
            return "resize failed";
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return "buffer failed";
        };
        buffer.fill(0x00ff_ffff);
        self.chrome.draw(&mut buffer, w.get(), h.get());
        if buffer.present().is_err() {
            return "present failed";
        }
        self.presented += 1;
        "ok"
    }
}

impl ApplicationHandler for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        trace("resumed enter");
        let attrs = Window::default_attributes()
            .with_title(self.options.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.options.width,
                self.options.height,
            ));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("cinder: could not create a window: {e}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cinder: no drawing context: {e}");
                event_loop.exit();
                return;
            }
        };
        trace("window created");
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cinder: no drawing surface: {e}");
                event_loop.exit();
                return;
            }
        };
        trace("surface ready");
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        // CEF's windowed mode embeds a child window, so the shell has to hand
        // over the platform handle: an X11 window id, an HWND, or an NSView.
        #[cfg(target_os = "linux")]
        let handle = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Xlib(h)) => h.window,
            Ok(RawWindowHandle::Xcb(h)) => h.window.get() as u64,
            Ok(other) => {
                eprintln!("cinder: unsupported window handle: {other:?}");
                event_loop.exit();
                return;
            }
            Err(e) => {
                eprintln!("cinder: no window handle: {e}");
                event_loop.exit();
                return;
            }
        };
        #[cfg(target_os = "windows")]
        let handle = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as u64,
            Ok(other) => {
                eprintln!("cinder: unsupported window handle: {other:?}");
                event_loop.exit();
                return;
            }
            Err(e) => {
                eprintln!("cinder: no window handle: {e}");
                event_loop.exit();
                return;
            }
        };
        #[cfg(target_os = "macos")]
        let handle = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as u64,
            Ok(other) => {
                eprintln!("cinder: unsupported window handle: {other:?}");
                event_loop.exit();
                return;
            }
            Err(e) => {
                eprintln!("cinder: no window handle: {e}");
                event_loop.exit();
                return;
            }
        };
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        let handle: u64 = 0;
        let view_height = size
            .height
            .saturating_sub(self.chrome.chrome_px())
            .saturating_sub(self.chrome.status_px())
            .max(1);
        let parent = NativeParent {
            handle,
            top: self.chrome.chrome_px(),
            width: size.width.max(1),
            height: view_height,
            scale,
        };
        self.handle = Some(handle);
        trace(&format!(
            "window ready: handle={handle:#x} size={}x{} view_height={view_height}",
            size.width, size.height
        ));
        let opened = self.open.take().and_then(|open| open(parent));
        let Some((key, view)) = opened else {
            eprintln!("cinder: could not create the page");
            event_loop.exit();
            return;
        };
        trace(&format!("view created: {view:?}"));
        self.view = Some(view);
        self.views.insert(key, view);
        self.keys.push(key);
        self.active = Some(key);
        self.last_size = (size.width, size.height);
        self.chrome.scale = scale;
        self.chrome.tabs = (self.tabs.list)()
            .into_iter()
            .map(|(_, chip)| chip)
            .collect();
        // A windowed browser is visible as soon as it is created; this only
        // makes the intent explicit and is harmless if CEF already showed it.
        let _ = self.engine.execute(Command::SetVisible {
            view,
            visible: true,
        });
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor() as f32)
                    .unwrap_or(1.0);
                self.resize_view(size.width, size.height, scale);
            }
            WindowEvent::RedrawRequested => {
                self.last_reason = self.present();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Focused(_) => {
                // CEF's child window owns focus for the page; the shell does not
                // forward focus changes.
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                let hit = self.hit(position.x, position.y);
                self.chrome.hover = hit;
            }
            WindowEvent::CursorLeft { .. } => {
                self.chrome.hover = None;
            }
            WindowEvent::MouseInput { state, .. } => {
                if let Some(hit) = self.hit(self.cursor.0, self.cursor.1) {
                    if state == ElementState::Pressed {
                        self.activate(hit);
                    }
                }
            }
            WindowEvent::MouseWheel { .. } => {
                // The page scrolls itself: the child window receives the event.
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.chrome.editing {
                    if event.state == ElementState::Pressed {
                        match self.chrome.key(&event.logical_key, self.modifiers) {
                            Edit::Navigate(text) => self.navigate(&text),
                            Edit::Cancel => {
                                if let Some(view) = self.view {
                                    let _ = self.engine.execute(Command::SetFocus {
                                        view,
                                        focused: true,
                                    });
                                }
                            }
                            Edit::None => {}
                        }
                    }
                    return;
                }
                if event.state == ElementState::Pressed {
                    let ctrl = self.modifiers.control_key();
                    let alt = self.modifiers.alt_key();
                    match &event.logical_key {
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("l") => {
                            self.chrome.begin_edit();
                            if let Some(view) = self.view {
                                let _ = self.engine.execute(Command::SetFocus {
                                    view,
                                    focused: false,
                                });
                            }
                        }
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("r") => {
                            self.reload();
                        }
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("t") => {
                            self.new_tab();
                        }
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("w") => {
                            if let Some(key) = self.active {
                                self.close_tab(key);
                            }
                        }
                        Key::Named(NamedKey::Tab) if ctrl && self.modifiers.shift_key() => {
                            self.cycle_tab(-1);
                        }
                        Key::Named(NamedKey::Tab) if ctrl => {
                            self.cycle_tab(1);
                        }
                        Key::Named(NamedKey::F5) => {
                            self.reload();
                        }
                        Key::Named(NamedKey::ArrowLeft) if alt => {
                            self.back();
                        }
                        Key::Named(NamedKey::ArrowRight) if alt => {
                            self.forward();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quit {
            event_loop.exit();
            return;
        }
        if self.selftest != 0 {
            trace(&format!("tick {} before pump", self.ticks));
        }
        // Inline engines only advance when the thread that owns them pumps.
        // On macOS CEF drives itself from the run loop instead, so this is a
        // no-op there; pumping it here would spin AppKit's loop and re-enter
        // winit's event handler.
        self.engine.pump();
        (self.tick)();
        if self.selftest != 0 {
            trace(&format!("tick {} after pump", self.ticks));
        }
        self.ticks += 1;
        // The strip is small and the host owns the model, so it is refreshed
        // every frame rather than pushed through a channel.
        self.chrome.tabs = (self.tabs.list)()
            .into_iter()
            .map(|(_, chip)| chip)
            .collect();
        // The address and the history flags come from the engine thread, so
        // they are polled rather than pushed. Each poll is a round trip that
        // waits for the engine's current pump slice, so it runs a few times a
        // second rather than on every frame.
        if self.ticks % 20 == 0 {
            if let Some(view) = self.view {
                if let Some(url) = self.engine.view_url(view) {
                    self.chrome.url = url;
                }
                self.chrome.back = self.engine.can_go_back(view);
                self.chrome.forward = self.engine.can_go_forward(view);
            }
        }
        if self.ticks % 30 == 0 {
            self.chrome.caret_on = !self.chrome.caret_on;
        }
        if let Ok(status) = self.ui.status.lock() {
            if self.chrome.status != *status {
                self.chrome.status = status.clone();
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        if self.selftest != 0 && self.ticks >= self.selftest {
            println!(
                "window selftest: presented={} view={} last={}",
                self.presented,
                match self.view {
                    Some(view) => view.to_string(),
                    None => "none".to_string(),
                },
                self.last_reason
            );
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    }
}

/// Turn address-field text into a URL: a bare host gets `https://`, and
/// anything that is not a host becomes a search.
fn normalize(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(text) {
        return Some(url);
    }
    if !text.contains(char::is_whitespace) && text.contains('.') {
        if let Ok(url) = Url::parse(&format!("https://{text}")) {
            return Some(url);
        }
    }
    let mut query = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                query.push(byte as char)
            }
            b' ' => query.push('+'),
            other => query.push_str(&format!("%{other:02X}")),
        }
    }
    Url::parse(&format!("https://duckduckgo.com/?q={query}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(
            normalize("example.com").map(|u| u.as_str().to_string()),
            Some("https://example.com".to_string())
        );
        assert_eq!(
            normalize("  https://example.com/a?b=1  ").map(|u| u.as_str().to_string()),
            Some("https://example.com/a?b=1".to_string())
        );
    }

    #[test]
    fn other_text_becomes_a_search() {
        let url = normalize("rust browser").map(|u| u.as_str().to_string()).unwrap_or_default();
        assert!(url.starts_with("https://duckduckgo.com/?q=rust+browser"), "{url}");
        let url = normalize("what?").map(|u| u.as_str().to_string()).unwrap_or_default();
        assert!(url.contains("what%3F"), "{url}");
    }

    #[test]
    fn empty_input_is_ignored() {
        assert!(normalize("   ").is_none());
    }
}
