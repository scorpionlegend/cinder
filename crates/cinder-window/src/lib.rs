//! The window layer: an OS window that shows CEF's off-screen frames and
//! replays OS input back into the page.
//!
//! CEF runs windowless here, so it never owns a native view and never sees an
//! OS event. Two consequences shape this crate:
//!
//!  * Pixels are pulled, not pushed. Each redraw copies the last painted frame
//!    out of the shim and blits it into the window's buffer.
//!  * Input is replayed. Every key, click and scroll is translated into the
//!    matching `InputEvent` and handed back to the engine thread.
//!
//! The event loop stays on the main thread, which is what winit requires; the
//! engine keeps its own thread and is reached only through channels.

mod chrome;
mod font;

use std::num::NonZeroU32;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinder_adapter::command::Command;
use cinder_adapter::engine::Engine;
use cinder_adapter::ids::ViewId;
use cinder_adapter::sanitize::Url;
use cinder_adapter_cef::{CefEngine, InputEvent, KeyKind, MouseButton};
use chrome::{Chrome, Edit, Hit};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// How often the loop wakes to pump the engine and repaint.
const TICK: Duration = Duration::from_millis(16);

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

/// Run the window until the user closes it. `tick` is called once per frame so
/// the caller can pump the orchestrator and react to effects.
pub fn run(
    engine: Arc<CefEngine>,
    view: ViewId,
    options: WindowOptions,
    ui: Ui,
    tick: impl FnMut() + 'static,
) -> Result<(), WindowError> {
    let event_loop = EventLoop::new().map_err(|e| WindowError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    let mut app = Shell {
        engine,
        view,
        options,
        ui,
        tick: Box::new(tick),
        window: None,
        surface: None,
        context: None,
        chrome: Chrome::new(1.0),
        in_chrome: false,
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        last_size: (0, 0),
        ticks: 0,
        presented: 0,
        last_frame: None,
        last_hist: None,
        content: false,
        last_reason: "none",
        selftest: std::env::var("CINDER_WINDOW_SELFTEST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| WindowError::EventLoop(e.to_string()))
}

struct Shell {
    engine: Arc<CefEngine>,
    view: ViewId,
    options: WindowOptions,
    ui: Ui,
    tick: Box<dyn FnMut()>,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    /// Toolbar, address field and status strip.
    chrome: Chrome,
    /// True while the pointer is over the chrome rather than the page.
    in_chrome: bool,
    modifiers: ModifiersState,
    cursor: (f64, f64),
    last_size: (u32, u32),
    ticks: u32,
    presented: u32,
    last_frame: Option<(u32, u32, usize)>,
    /// The most common colours in the last frame, for the selftest report.
    last_hist: Option<String>,
    /// Set once a frame arrives that is neither blank nor uniform.
    content: bool,
    /// Why the last redraw did or did not present, for the selftest report.
    last_reason: &'static str,
    /// Set by `CINDER_WINDOW_SELFTEST`: run for a bounded number of frames and
    /// report what was presented, so the window path can be checked without a
    /// human looking at the screen. Zero means run until closed.
    selftest: u32,
}

impl Shell {
    fn resize_view(&mut self, width: u32, height: u32, scale: f32) {
        if width == 0 || height == 0 || (width, height) == self.last_size {
            return;
        }
        self.last_size = (width, height);
        self.chrome.scale = scale;
        let view_height = height
            .saturating_sub(self.chrome.toolbar_px())
            .saturating_sub(self.chrome.status_px())
            .max(1);
        let _ = self.engine.execute(Command::Resize {
            view: self.view,
            width,
            height: view_height,
            scale,
        });
    }

    fn input(&self, event: InputEvent) {
        self.engine.send_input(self.view, event);
    }

    /// Tell the page the pointer left it, once, when it moves into the chrome.
    fn page_leave(&self) {
        self.input(InputEvent::MouseMove {
            x: self.cursor.0 as i32,
            y: (self.cursor.1 - self.chrome.toolbar_px() as f64) as i32,
            modifiers: self.modifiers(),
            leave: true,
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
                let _ = self.engine.execute(Command::SetFocus {
                    view: self.view,
                    focused: false,
                });
            }
            Hit::Status => {
                if let Some(clicked) = self.ui.clicked.as_ref() {
                    let _ = clicked.send(());
                }
            }
        }
    }

    fn back(&mut self) {
        let _ = self.engine.execute(Command::GoBack { view: self.view });
    }

    fn forward(&mut self) {
        let _ = self.engine.execute(Command::GoForward { view: self.view });
    }

    fn reload(&mut self) {
        let _ = self.engine.execute(Command::Reload {
            view: self.view,
            bypass_cache: false,
        });
    }

    /// Navigate to whatever the address field holds.
    fn navigate(&mut self, text: &str) {
        let Some(url) = normalize(text) else {
            return;
        };
        self.chrome.cancel_edit();
        let _ = self.engine.execute(Command::SetFocus {
            view: self.view,
            focused: true,
        });
        let _ = self.engine.execute(Command::Navigate {
            view: self.view,
            url,
        });
    }

    fn present(&mut self) -> &'static str {
        let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref()) else {
            return "no surface";
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return "zero size";
        };
        let Some(frame) = self.engine.copy_frame(self.view) else {
            return "no frame";
        };
        if surface.resize(w, h).is_err() {
            return "resize failed";
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return "buffer failed";
        };
        let width = w.get() as usize;
        let height = h.get() as usize;
        let top = self.chrome.toolbar_px() as usize;
        let bottom = height.saturating_sub(self.chrome.status_px() as usize);
        buffer.fill(0x00ff_ffff);
        // The frame is BGRA and the buffer is 0x00RRGGBB, so the channels swap
        // and alpha is dropped: CEF always paints opaque. It lands below the
        // toolbar and above the status strip.
        let stride = frame.width as usize * 4;
        let cols = (frame.width as usize).min(width);
        let rows = (frame.height as usize).min(bottom.saturating_sub(top));
        for y in 0..rows {
            let src = &frame.bgra[y * stride..y * stride + cols * 4];
            let base = (top + y) * width;
            let dst = &mut buffer[base..base + cols];
            for (out, px) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *out = ((px[2] as u32) << 16) | ((px[1] as u32) << 8) | px[0] as u32;
            }
        }
        self.chrome.draw(&mut buffer, w.get(), h.get());
        let presented = buffer.present();
        if presented.is_err() {
            return "present failed";
        }
        self.presented += 1;
        let non_white = frame
            .bgra
            .chunks_exact(4)
            .filter(|p| !(p[0] >= 250 && p[1] >= 250 && p[2] >= 250))
            .count();
        self.last_frame = Some((frame.width, frame.height, non_white));
        let total = frame.width as usize * frame.height as usize;
        if non_white > 0 && non_white < total {
            self.content = true;
        }
        if self.selftest != 0 && self.ticks + 1 >= self.selftest {
            self.last_hist = Some(histogram(&frame.bgra));
        }
        if let Some(path) = std::env::var_os("CINDER_WINDOW_DUMP") {
            if self.presented % 60 == 0 {
                let _ = std::fs::write(&path, &frame.bgra);
                println!(
                    "window dump: {}x{} non_white={} -> {}",
                    frame.width,
                    frame.height,
                    non_white,
                    std::path::Path::new(&path).display()
                );
            }
        }
        "ok"
    }

    fn modifiers(&self) -> u32 {
        let mut flags = 0u32;
        if self.modifiers.shift_key() {
            flags |= 1 << 1;
        }
        if self.modifiers.control_key() {
            flags |= 1 << 2;
        }
        if self.modifiers.alt_key() {
            flags |= 1 << 3;
        }
        flags
    }
}

impl ApplicationHandler for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
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
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cinder: no drawing surface: {e}");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        self.resize_view(size.width, size.height, scale);
        // Windowless views start hidden; without an explicit show the
        // compositor never produces a frame.
        let _ = self.engine.execute(Command::SetVisible {
            view: self.view,
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
            WindowEvent::Focused(focused) => {
                let _ = self.engine.execute(Command::SetFocus {
                    view: self.view,
                    focused,
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                let hit = self.hit(position.x, position.y);
                self.chrome.hover = hit;
                if hit.is_some() {
                    if !self.in_chrome {
                        self.in_chrome = true;
                        self.page_leave();
                    }
                } else {
                    self.in_chrome = false;
                    self.input(InputEvent::MouseMove {
                        x: position.x as i32,
                        y: (position.y - self.chrome.toolbar_px() as f64) as i32,
                        modifiers: self.modifiers(),
                        leave: false,
                    });
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.chrome.hover = None;
                self.in_chrome = true;
                self.page_leave();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(hit) = self.hit(self.cursor.0, self.cursor.1) {
                    if state == ElementState::Pressed {
                        self.activate(hit);
                    }
                    return;
                }
                let button = match button {
                    WinitButton::Left => MouseButton::Left,
                    WinitButton::Middle => MouseButton::Middle,
                    WinitButton::Right => MouseButton::Right,
                    _ => return,
                };
                self.input(InputEvent::MouseClick {
                    x: self.cursor.0 as i32,
                    y: (self.cursor.1 - self.chrome.toolbar_px() as f64) as i32,
                    modifiers: self.modifiers(),
                    button,
                    up: state == ElementState::Released,
                    click_count: 1,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.in_chrome {
                    return;
                }
                // A line is roughly 40 DIP; CEF wants pixel deltas.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        ((x * 40.0) as i32, (y * 40.0) as i32)
                    }
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                self.input(InputEvent::MouseWheel {
                    x: self.cursor.0 as i32,
                    y: (self.cursor.1 - self.chrome.toolbar_px() as f64) as i32,
                    modifiers: self.modifiers(),
                    delta_x: dx,
                    delta_y: dy,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.chrome.editing {
                    if event.state == ElementState::Pressed {
                        match self.chrome.key(&event.logical_key, self.modifiers) {
                            Edit::Navigate(text) => self.navigate(&text),
                            Edit::Cancel => {
                                let _ = self.engine.execute(Command::SetFocus {
                                    view: self.view,
                                    focused: true,
                                });
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
                            let _ = self.engine.execute(Command::SetFocus {
                                view: self.view,
                                focused: false,
                            });
                            return;
                        }
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("r") => {
                            self.reload();
                            return;
                        }
                        Key::Named(NamedKey::F5) => {
                            self.reload();
                            return;
                        }
                        Key::Named(NamedKey::ArrowLeft) if alt => {
                            self.back();
                            return;
                        }
                        Key::Named(NamedKey::ArrowRight) if alt => {
                            self.forward();
                            return;
                        }
                        _ => {}
                    }
                }
                let (code, character) = key_code(&event.logical_key);
                let modifiers = self.modifiers();
                let kind = match event.state {
                    ElementState::Pressed => KeyKind::RawKeyDown,
                    ElementState::Released => KeyKind::KeyUp,
                };
                self.input(InputEvent::Key {
                    kind,
                    modifiers,
                    windows_key_code: code,
                    native_key_code: code,
                    is_system_key: false,
                    character,
                    unmodified_character: character,
                });
                // A printable key also produces a character event, which is what
                // actually inserts text into a field.
                if event.state == ElementState::Pressed && character != 0 {
                    self.input(InputEvent::Key {
                        kind: KeyKind::Char,
                        modifiers,
                        windows_key_code: code,
                        native_key_code: code,
                        is_system_key: false,
                        character,
                        unmodified_character: character,
                    });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        (self.tick)();
        self.ticks += 1;
        // The address and the history flags come from the engine thread, so
        // they are polled rather than pushed. Each poll is a round trip that
        // waits for the engine's current pump slice, so it runs a few times a
        // second rather than on every frame.
        if self.ticks % 20 == 0 {
            if let Some(url) = self.engine.view_url(self.view) {
                self.chrome.url = url;
            }
            self.chrome.back = self.engine.can_go_back(self.view);
            self.chrome.forward = self.engine.can_go_forward(self.view);
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
        if self.selftest != 0 && (self.ticks >= self.selftest || self.content) {
            match self.last_frame {
                Some((w, h, non_white)) => {
                    println!(
                        "window selftest: presented={} frame={}x{} non_white={} last={}",
                        self.presented, w, h, non_white, self.last_reason
                    );
                    if let Some(hist) = self.last_hist.as_ref() {
                        println!("window selftest: top colors {hist}");
                    }
                }
                None => println!(
                    "window selftest: presented={} no frame last={}",
                    self.presented, self.last_reason
                ),
            }
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


/// The eight most common colours in a BGRA frame, as `#rrggbb:count`.
fn histogram(bgra: &[u8]) -> String {
    use std::collections::HashMap;
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for px in bgra.chunks_exact(4) {
        let key = ((px[2] as u32) << 16) | ((px[1] as u32) << 8) | px[0] as u32;
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut top: Vec<(u32, usize)> = counts.into_iter().collect();
    top.sort_by_key(|a| std::cmp::Reverse(a.1));
    top.into_iter()
        .take(8)
        .map(|(c, n)| format!("#{c:06x}:{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map a winit key to a Windows virtual-key code and, for printable keys, the
/// UTF-16 unit CEF expects in `character`.
fn key_code(key: &Key) -> (i32, u32) {
    match key {
        Key::Named(named) => {
            let code = match named {
                NamedKey::Enter => 13,
                NamedKey::Backspace => 8,
                NamedKey::Tab => 9,
                NamedKey::Escape => 27,
                NamedKey::Space => 32,
                NamedKey::PageUp => 33,
                NamedKey::PageDown => 34,
                NamedKey::End => 35,
                NamedKey::Home => 36,
                NamedKey::ArrowLeft => 37,
                NamedKey::ArrowUp => 38,
                NamedKey::ArrowRight => 39,
                NamedKey::ArrowDown => 40,
                NamedKey::Delete => 46,
                _ => 0,
            };
            (code, 0)
        }
        Key::Character(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    let upper = c.to_ascii_uppercase() as i32;
                    (upper, c as u32)
                }
                _ => (0, 0),
            }
        }
        _ => (0, 0),
    }
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
