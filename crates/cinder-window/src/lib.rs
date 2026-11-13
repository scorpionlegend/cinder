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

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cinder_adapter::command::Command;
use cinder_adapter::engine::Engine;
use cinder_adapter::ids::ViewId;
use cinder_adapter_cef::{CefEngine, InputEvent, KeyKind, MouseButton};
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

/// Run the window until the user closes it. `tick` is called once per frame so
/// the caller can pump the orchestrator and react to effects.
pub fn run(
    engine: Arc<CefEngine>,
    view: ViewId,
    options: WindowOptions,
    tick: impl FnMut() + 'static,
) -> Result<(), WindowError> {
    let event_loop = EventLoop::new().map_err(|e| WindowError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    let mut app = Shell {
        engine,
        view,
        options,
        tick: Box::new(tick),
        window: None,
        surface: None,
        context: None,
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
            .unwrap_or(300),
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| WindowError::EventLoop(e.to_string()))
}

struct Shell {
    engine: Arc<CefEngine>,
    view: ViewId,
    options: WindowOptions,
    tick: Box<dyn FnMut()>,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
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
        let _ = self.engine.execute(Command::Resize {
            view: self.view,
            width,
            height,
            scale,
        });
    }

    fn input(&self, event: InputEvent) {
        self.engine.send_input(self.view, event);
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
        // The frame is BGRA and the buffer is 0x00RRGGBB, so the channels swap
        // and alpha is dropped: CEF always paints opaque.
        let stride = frame.width as usize * 4;
        let rows = (frame.height as usize).min(h.get() as usize);
        let cols = (frame.width as usize).min(w.get() as usize);
        let width = w.get() as usize;
        for y in 0..rows {
            let src = &frame.bgra[y * stride..y * stride + cols * 4];
            let base = y * width;
            let dst = &mut buffer[base..base + cols];
            for (out, px) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *out = ((px[2] as u32) << 16) | ((px[1] as u32) << 8) | px[0] as u32;
            }
        }
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
                self.input(InputEvent::MouseMove {
                    x: position.x as i32,
                    y: position.y as i32,
                    modifiers: self.modifiers(),
                    leave: false,
                });
            }
            WindowEvent::CursorLeft { .. } => {
                self.input(InputEvent::MouseMove {
                    x: self.cursor.0 as i32,
                    y: self.cursor.1 as i32,
                    modifiers: self.modifiers(),
                    leave: true,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    WinitButton::Left => MouseButton::Left,
                    WinitButton::Middle => MouseButton::Middle,
                    WinitButton::Right => MouseButton::Right,
                    _ => return,
                };
                self.input(InputEvent::MouseClick {
                    x: self.cursor.0 as i32,
                    y: self.cursor.1 as i32,
                    modifiers: self.modifiers(),
                    button,
                    up: state == ElementState::Released,
                    click_count: 1,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // A line is roughly 40 DIP; CEF wants pixel deltas.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        ((x * 40.0) as i32, (y * 40.0) as i32)
                    }
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                self.input(InputEvent::MouseWheel {
                    x: self.cursor.0 as i32,
                    y: self.cursor.1 as i32,
                    modifiers: self.modifiers(),
                    delta_x: dx,
                    delta_y: dy,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        self.ticks += 1;
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
