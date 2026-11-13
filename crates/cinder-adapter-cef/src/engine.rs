//! `Engine` implementation over the CEF shim.
//!
//! CEF requires every call — initialize, browser creation, the message loop,
//! shutdown — on one thread. `Engine::execute` is called from the orchestrator's
//! thread, so this module owns a dedicated engine thread: commands arrive over a
//! channel, the thread pumps CEF, and events leave through the shim's callback
//! into the same `EventStream` the mock engine uses.
//!
//! Nothing here blocks on CEF from the caller's side: `execute` waits for the
//! engine thread to accept the command, not for the command's effects.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uchar, c_uint, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cinder_adapter::capability::Capabilities;
use cinder_adapter::command::{Command, CommandOutcome, Surface};
use cinder_adapter::engine::Engine;
use cinder_adapter::error::AdapterError;
use cinder_adapter::event::{EngineMessage, Event, EventEnvelope, EventStream, LoadFailure};
use cinder_adapter::ids::{IdAllocator, ProfileId, SeqNo, ViewId};
use cinder_adapter::policy::PolicySnapshot;
use cinder_adapter::sanitize::Url;
use cinder_adapter::view_state::ViewState;

use crate::{Cef, CefError, Frame, STATE_CLOSED, STATE_CREATED};

mod ffi {
    use super::*;

    extern "C" {
        pub fn cinder_cef_set_event_callback(
            cb: Option<extern "C" fn(*mut c_void, c_int, c_int, *const c_char, *const c_char)>,
            user_data: *mut c_void,
        );
        pub fn cinder_cef_create_view(
            view_id: c_int,
            url: *const c_char,
            width: c_int,
            height: c_int,
            parent: u64,
            top: c_int,
        ) -> c_int;
        pub fn cinder_cef_close_view(view_id: c_int, force: c_int) -> c_int;
        pub fn cinder_cef_navigate(view_id: c_int, url: *const c_char) -> c_int;
        pub fn cinder_cef_reload(view_id: c_int) -> c_int;
        pub fn cinder_cef_stop(view_id: c_int) -> c_int;
        pub fn cinder_cef_go_back(view_id: c_int) -> c_int;
        pub fn cinder_cef_go_forward(view_id: c_int) -> c_int;
        pub fn cinder_cef_can_go_back(view_id: c_int) -> c_int;
        pub fn cinder_cef_can_go_forward(view_id: c_int) -> c_int;
        pub fn cinder_cef_view_url(view_id: c_int) -> *const c_char;
        pub fn cinder_cef_set_zoom(view_id: c_int, level: f64) -> c_int;
        pub fn cinder_cef_set_mute(view_id: c_int, muted: c_int) -> c_int;
        pub fn cinder_cef_find(view_id: c_int, query: *const c_char, forward: c_int) -> c_int;
        pub fn cinder_cef_resize(view_id: c_int, width: c_int, height: c_int) -> c_int;
        pub fn cinder_cef_set_visible(view_id: c_int, visible: c_int) -> c_int;
        pub fn cinder_cef_set_focus(view_id: c_int, focused: c_int) -> c_int;
        pub fn cinder_cef_view_state(view_id: c_int) -> c_int;
        pub fn cinder_cef_view_frame_size(
            view_id: c_int,
            width: *mut c_int,
            height: *mut c_int,
        ) -> c_int;
        pub fn cinder_cef_copy_view_frame(
            view_id: c_int,
            dst: *mut c_uchar,
            capacity: c_int,
        ) -> c_int;
        pub fn cinder_cef_send_mouse_move(
            view_id: c_int,
            x: c_int,
            y: c_int,
            modifiers: c_uint,
            leave: c_int,
        );
        pub fn cinder_cef_send_mouse_click(
            view_id: c_int,
            x: c_int,
            y: c_int,
            modifiers: c_uint,
            button: c_int,
            up: c_int,
            click_count: c_int,
        );
        pub fn cinder_cef_send_mouse_wheel(
            view_id: c_int,
            x: c_int,
            y: c_int,
            modifiers: c_uint,
            delta_x: c_int,
            delta_y: c_int,
        );
        pub fn cinder_cef_send_key(
            view_id: c_int,
            kind: c_int,
            modifiers: c_uint,
            windows_key_code: c_int,
            native_key_code: c_int,
            is_system_key: c_int,
            character: c_uint,
            unmodified_character: c_uint,
        );
        pub fn cinder_cef_set_policy(
            blocked_hosts: *const *const c_char,
            count: c_int,
            upgrade_https: c_int,
        ) -> c_int;
    }
}

/// How long one `pump` slice runs before commands are serviced again.
/// Progress notes for `CINDER_WINDOW_SELFTEST`, matching the window layer's
/// `window trace:` lines so one log reads as a single sequence.
fn trace(step: &str) {
    if std::env::var_os("CINDER_WINDOW_SELFTEST").is_some() {
        eprintln!("window trace: engine {step}");
    }
}

/// Marks the engine core as busy on this thread.
///
/// Pumping CEF runs the platform message loop, which can re-enter the host's
/// window procedure and call back into the engine. The core mutex is not
/// reentrant, so a nested call would deadlock; it is refused instead.
struct CoreBusy;

thread_local! {
    static IN_CORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl CoreBusy {
    fn enter() -> Option<Self> {
        IN_CORE.with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(Self)
            }
        })
    }
}

impl Drop for CoreBusy {
    fn drop(&mut self) {
        IN_CORE.with(|flag| flag.set(false));
    }
}

const PUMP_MS: u32 = 5;/// How long `execute` waits for the engine thread to accept a command.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long view creation waits for CEF's `on_after_created`.
const CREATE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for views to confirm closure.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a frame copy or input forward waits for the engine thread.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);

/// Where CEF finds its resources and where it may write its cache.
#[derive(Debug, Clone)]
pub struct CefEngineConfig {
    pub resources_dir: PathBuf,
    pub locales_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Extra Chromium switches, appended to the command line.
    pub extra_args: Vec<String>,
    /// Whether this process paints off-screen. A windowed shell sets this to
    /// `false` so CEF creates native child windows instead.
    pub windowless: bool,
    /// Run CEF on the calling thread instead of a dedicated one.
    ///
    /// Chromium binds its browser UI thread to the process's main thread on
    /// macOS and Windows; initializing anywhere else leaves every later call on
    /// an "Unknown Thread" and trips a `CHECK` inside the first pump. Linux has
    /// no such binding, so it keeps the dedicated thread.
    pub main_thread: bool,
    /// Whether the host pumps CEF with `CefDoMessageLoopWork`.
    ///
    /// A host that already runs an event loop on the initializing thread asks
    /// for this. macOS is the exception: pumping there spins AppKit's run loop,
    /// which re-enters the host's event handler, so CEF drives itself from the
    /// run loop instead and [`Engine::pump`] does nothing.
    pub external_pump: bool,
    /// Whether this process pumps CEF from its own event loop.
    ///
    /// False on macOS, where the shim drives `cef_do_message_loop_work` from a
    /// run loop timer: pumping from inside the host's callback would re-enter
    /// the host's event handler.
    pub host_pumps: bool,
    /// Whether a command may pump CEF inline, i.e. block until CEF reports the
    /// browser. Only Linux may: on Windows and macOS pumping from inside the
    /// host's event callback runs a nested platform message loop that never
    /// returns.
    pub pump_in_send: bool,
    /// Whether a hidden timer window drives `cef_do_message_loop_work` instead
    /// of the host's event loop. Windows only; see
    /// [`default_pump_timer`].
    pub pump_timer: bool,
    /// What this build actually provides. Defaults to [`measured_capabilities`].
    pub capabilities: Capabilities,
}

impl CefEngineConfig {
    /// Resources and locales next to the running executable, cache under the
    /// system temp directory. This is the layout `cinder-dist` produces.
    pub fn beside_executable() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        let dir = exe.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let cache = std::env::temp_dir().join("cinder-cache");
        Self {
            resources_dir: dir.clone(),
            locales_dir: dir.join("locales"),
            cache_dir: cache,
            extra_args: Vec::new(),
            windowless: true,
            main_thread: default_main_thread(),
            external_pump: default_external_pump(),
            host_pumps: default_host_pumps(),
            pump_in_send: default_pump_in_send(),
            pump_timer: default_pump_timer(),
            capabilities: measured_capabilities(),
        }
    }
}

/// Whether CEF has to run on the caller's thread on this platform.
///
/// `CINDER_CEF_MAIN_THREAD=1` forces the inline path and `=0` the threaded one,
/// so either can be exercised on any platform.
pub fn default_main_thread() -> bool {
    match std::env::var("CINDER_CEF_MAIN_THREAD") {
        Ok(value) => value != "0",
        Err(_) => !cfg!(target_os = "linux"),
    }
}

/// Whether the host has to pump CEF itself. `CINDER_CEF_EXTERNAL_PUMP=0|1`
/// overrides it so either mode can be exercised on any platform.
pub fn default_external_pump() -> bool {
    match std::env::var("CINDER_CEF_EXTERNAL_PUMP") {
        Ok(value) => value != "0",
        Err(_) => true,
    }
}

/// Whether this process pumps CEF itself. `CINDER_CEF_HOST_PUMP=0|1` overrides
/// it so either mode can be exercised on any platform.
pub fn default_host_pumps() -> bool {
    match std::env::var("CINDER_CEF_HOST_PUMP") {
        Ok(value) => value != "0",
        Err(_) => !cfg!(target_os = "macos"),
    }
}

/// Whether a command may pump CEF inline. `CINDER_CEF_PUMP_IN_SEND=0|1`
/// overrides it so either mode can be exercised on any platform.
pub fn default_pump_in_send() -> bool {
    match std::env::var("CINDER_CEF_PUMP_IN_SEND") {
        Ok(value) => value != "0",
        Err(_) => cfg!(target_os = "linux"),
    }
}

/// Whether a hidden timer window drives the pump. `CINDER_CEF_PUMP_TIMER=0|1`
/// overrides it; the shim only has such a window on Windows.
///
/// Windows defaults to it: pumping from inside winit's event callback runs a
/// nested Win32 message loop that never returns, so the window selftest stalls
/// at its first tick. A `WM_TIMER` on a message-only window is dispatched by
/// whichever loop is running and needs no re-entry.
pub fn default_pump_timer() -> bool {
    match std::env::var("CINDER_CEF_PUMP_TIMER") {
        Ok(value) => value != "0",
        Err(_) => cfg!(target_os = "windows"),
    }
}

/// The capability set this adapter actually implements.
///
/// Every `true` here is backed by a shim entry point that the engine calls. It
/// is deliberately narrower than [`Capabilities::cef_baseline_expected`]: that
/// constant records what CEF is *expected* to offer, not what Cinder wires up.
pub const fn measured_capabilities() -> Capabilities {
    Capabilities {
        incognito_contexts: false,
        offscreen_rendering: true,
        download_pause: false,
        print_to_pdf: false,
        find_in_page: true,
        devtools: false,
        request_interception: true,
    }
}

enum EngineRequest {
    Command {
        command: Command,
        ack: Sender<Result<CommandOutcome, AdapterError>>,
    },
    /// Copy the last painted frame. Windowless rendering means the shell has to
    /// pull pixels; CEF will not push them anywhere else.
    Frame {
        view: ViewId,
        ack: Sender<Option<Frame>>,
    },
    /// Forward one OS input event into the page.
    Input {
        view: ViewId,
        event: InputEvent,
        ack: Sender<bool>,
    },
    /// The view's current URL, for the shell's address bar.
    Url {
        view: ViewId,
        ack: Sender<Option<String>>,
    },
    /// Whether history can move back (`back`) or forward.
    History {
        view: ViewId,
        back: bool,
        ack: Sender<bool>,
    },
}

/// One input event from the shell's window. Windowless rendering hides OS
/// events from CEF entirely, so each one has to be replayed here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    MouseMove {
        x: i32,
        y: i32,
        modifiers: u32,
        leave: bool,
    },
    MouseClick {
        x: i32,
        y: i32,
        modifiers: u32,
        button: MouseButton,
        up: bool,
        click_count: i32,
    },
    MouseWheel {
        x: i32,
        y: i32,
        modifiers: u32,
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        kind: KeyKind,
        modifiers: u32,
        windows_key_code: i32,
        native_key_code: i32,
        is_system_key: bool,
        character: u32,
        unmodified_character: u32,
    },
}

/// `cef_mouse_button_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
}

/// `cef_key_event_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    RawKeyDown = 0,
    KeyDown = 1,
    KeyUp = 2,
    Char = 3,
}

/// Shared with the C callback. Lives on the engine thread's stack frame and is
/// handed to C as a raw pointer for the lifetime of the CEF session.
struct EventSink {
    tx: Sender<EngineMessage>,
    seq: Mutex<HashMap<ViewId, SeqNo>>,
    /// Shim view id → adapter view id.
    views: Mutex<HashMap<i32, ViewId>>,
}

impl EventSink {
    fn new(tx: Sender<EngineMessage>) -> Self {
        Self {
            tx,
            seq: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
        }
    }

    fn bind(&self, raw: i32, view: ViewId) {
        self.views.lock().unwrap_or_else(|e| e.into_inner()).insert(raw, view);
    }

    fn unbind(&self, raw: i32) {
        self.views.lock().unwrap_or_else(|e| e.into_inner()).remove(&raw);
    }

    fn view_of(&self, raw: i32) -> Option<ViewId> {
        self.views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&raw)
            .copied()
    }

    fn emit(&self, view: Option<ViewId>, event: Event) {
        let seq = match view {
            Some(v) => {
                let mut map = self.seq.lock().unwrap_or_else(|e| e.into_inner());
                let entry = map.entry(v).or_default();
                let current = *entry;
                *entry = current.next();
                current
            }
            None => SeqNo::default(),
        };
        let _ = self
            .tx
            .send(EngineMessage::Event(EventEnvelope::new(seq, event)));
    }
}

fn c_string(value: &str, what: &'static str) -> Result<CString, AdapterError> {
    CString::new(value).map_err(|_| AdapterError::InvalidArgument(what))
}

fn load_failure(code: i32) -> LoadFailure {
    match code {
        -3 => LoadFailure::Aborted,          // ERR_ABORTED
        -7 => LoadFailure::TimedOut,         // ERR_TIMED_OUT
        -105 | -106 => LoadFailure::ConnectionFailed, // NAME_NOT_RESOLVED, CONNECTION_FAILED
        -299..=-200 => LoadFailure::CertificateError,
        _ => LoadFailure::Unknown,
    }
}

/// CEF callback entry point. Runs on the engine thread inside `pump`.
extern "C" fn on_cef_event(
    user_data: *mut c_void,
    view_id: c_int,
    kind: c_int,
    a: *const c_char,
    b: *const c_char,
) {
    if user_data.is_null() {
        return;
    }
    // A panic must never unwind into C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `user_data` is the `Arc<EventSink>` pointer registered in
        // `run_engine`, which outlives every CEF callback.
        let sink = unsafe { &*(user_data as *const EventSink) };
        let Some(view) = sink.view_of(view_id) else {
            return;
        };
        let text = |p: *const c_char| -> String {
            if p.is_null() {
                String::new()
            } else {
                // SAFETY: the shim passes NUL-terminated buffers valid for the
                // duration of this call.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };

        match kind {
            crate::EV_CREATED => {
                sink.emit(Some(view), Event::ViewCreated { view });
                sink.emit(Some(view), Event::ViewReady { view });
            }
            crate::EV_LOAD_START => {
                if let Ok(url) = Url::parse(&text(a)) {
                    sink.emit(Some(view), Event::NavigationStarted { view, url });
                }
                sink.emit(Some(view), Event::LoadProgress { view, fraction: 0.0 });
            }
            crate::EV_LOAD_END => {
                sink.emit(Some(view), Event::LoadProgress { view, fraction: 1.0 });
                sink.emit(Some(view), Event::LoadFinished { view });
            }
            crate::EV_LOAD_ERROR => {
                let code = text(a).parse::<i32>().unwrap_or(0);
                let failed = text(b);
                if let Ok(url) = Url::parse(&failed) {
                    sink.emit(Some(view), Event::UrlChanged { view, url });
                }
                sink.emit(
                    Some(view),
                    Event::LoadFailed {
                        view,
                        reason: load_failure(code),
                    },
                );
            }
            crate::EV_TITLE => {
                sink.emit(Some(view), Event::TitleChanged { view, title: text(a) });
            }
            crate::EV_URL => {
                if let Ok(url) = Url::parse(&text(a)) {
                    sink.emit(Some(view), Event::UrlChanged { view, url });
                }
            }
            crate::EV_BLOCKED => {
                if let Ok(url) = Url::parse(&text(a)) {
                    sink.emit(Some(view), Event::UrlChanged { view, url });
                }
                sink.emit(
                    Some(view),
                    Event::LoadFailed {
                        view,
                        reason: LoadFailure::BlockedByPolicy,
                    },
                );
            }
            crate::EV_DOWNLOAD => {
                if let Ok(url) = Url::parse(&text(a)) {
                    sink.emit(
                        Some(view),
                        Event::DownloadStarted {
                            view,
                            url,
                            name: text(b),
                        },
                    );
                }
            }
            crate::EV_DOWNLOAD_UPDATED => {
                let state = text(b);
                let mut parts = state.split(':');
                let percent = parts
                    .next()
                    .and_then(|p| p.parse::<i32>().ok())
                    .unwrap_or(0)
                    .clamp(0, 100) as u8;
                let flag = |p: Option<&str>| p == Some("1");
                sink.emit(
                    Some(view),
                    Event::DownloadProgress {
                        view,
                        path: text(a),
                        percent,
                        done: flag(parts.next()),
                        canceled: flag(parts.next()),
                        interrupted: flag(parts.next()),
                    },
                );
            }
            crate::EV_CLOSE_REQUESTED => {
                sink.emit(Some(view), Event::CloseRequested { view });
            }
            crate::EV_CLOSED => {
                sink.emit(Some(view), Event::ViewClosed { view });
            }
            _ => {}
        }
    }));
}

#[derive(Debug, Default)]
struct EngineState {
    profiles: HashMap<ProfileId, bool>,
    views: HashMap<ViewId, ViewState>,
    policy: Option<Arc<PolicySnapshot>>,
    closed: bool,
}

impl EngineState {
    fn gate(&self, view: ViewId) -> Result<(), AdapterError> {
        match self.views.get(&view) {
            Some(ViewState::Live) => Ok(()),
            Some(ViewState::Creating) => Err(AdapterError::NotInitialized),
            Some(ViewState::Closing) => Err(AdapterError::ViewClosing),
            Some(ViewState::Closed) => Err(AdapterError::ViewClosed),
            Some(ViewState::Failed) => Err(AdapterError::EngineFault("view failed".into())),
            None => Err(AdapterError::ViewClosed),
        }
    }
}

/// A CEF-backed engine. One per process: CEF permits a single initialization.
pub struct CefEngine {
    /// Command channel to the engine thread. `None` when CEF runs inline.
    tx: Mutex<Option<Sender<EngineRequest>>>,
    rx: Mutex<Option<Receiver<EngineMessage>>>,
    /// The CEF session, owned here when [`CefEngineConfig::main_thread`] is set.
    core: Mutex<Option<EngineCore>>,
    capabilities: Capabilities,
    closed: AtomicBool,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for CefEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CefEngine")
            .field("capabilities", &self.capabilities)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl CefEngine {
    /// Start CEF on a dedicated thread. Blocks until initialization succeeds or
    /// fails, so a broken runtime is reported here rather than on first use.
    pub fn new(config: CefEngineConfig) -> Result<Self, CefError> {
        let (event_tx, event_rx) = channel();
        let capabilities = config.capabilities;

        if config.main_thread {
            let core = EngineCore::start(&config, event_tx)?;
            return Ok(Self {
                tx: Mutex::new(None),
                rx: Mutex::new(Some(event_rx)),
                core: Mutex::new(Some(core)),
                capabilities,
                closed: AtomicBool::new(false),
                join: Mutex::new(None),
            });
        }

        let (cmd_tx, cmd_rx) = channel();
        let (init_tx, init_rx) = channel();
        let join = std::thread::Builder::new()
            .name("cinder-cef".into())
            .spawn(move || run_engine(config, cmd_rx, event_tx, init_tx))
            .map_err(|e| CefError::InitFailed(format!("engine thread: {e}")))?;

        match init_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => Ok(Self {
                tx: Mutex::new(Some(cmd_tx)),
                rx: Mutex::new(Some(event_rx)),
                core: Mutex::new(None),
                capabilities,
                closed: AtomicBool::new(false),
                join: Mutex::new(Some(join)),
            }),
            Ok(Err(message)) => {
                let _ = join.join();
                Err(CefError::InitFailed(message))
            }
            Err(_) => Err(CefError::InitFailed("engine thread did not report".into())),
        }
    }

    fn send(&self, command: Command) -> Result<CommandOutcome, AdapterError> {
        let (ack_tx, ack_rx) = channel();
        let request = EngineRequest::Command {
            command,
            ack: ack_tx,
        };
        // Pumping CEF runs the platform message loop, which can re-enter the
        // host's window procedure and call back into the engine. The core mutex
        // is not reentrant, so a nested call is refused instead of deadlocking.
        let Some(_busy) = CoreBusy::enter() else {
            return Err(AdapterError::EngineFault("engine re-entered".into()));
        };
        trace("send enter");
        let mut guard = self.core.lock().unwrap_or_else(|e| e.into_inner());
        trace("send locked");
        if let Some(core) = guard.as_mut() {
            if core.pump_in_send {
                core.pump();
            }
            trace("send pumped");
            let alive = core.serve(request);
            trace("send served");
            let result = ack_rx.recv_timeout(ACCEPT_TIMEOUT).unwrap_or_else(|_| {
                Err(AdapterError::EngineFault(
                    "engine did not accept the command".into(),
                ))
            });
            if !alive {
                if let Some(core) = guard.take() {
                    core.finish();
                }
            }
            return result;
        }
        drop(guard);
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        tx.as_ref()
            .ok_or_else(|| AdapterError::EngineFault("engine is shut down".into()))?
            .send(request)
            .map_err(|_| AdapterError::EngineFault("engine thread is gone".into()))?;
        drop(tx);
        match ack_rx.recv_timeout(ACCEPT_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(AdapterError::EngineFault(
                "engine thread did not accept the command".into(),
            )),
        }
    }

    /// Run one read-only request, inline or over the engine thread's channel.
    fn request<T>(
        &self,
        make: impl FnOnce(Sender<T>) -> EngineRequest,
        timeout: Duration,
    ) -> Option<T> {
        let (ack_tx, ack_rx) = channel();
        let request = make(ack_tx);
        let _busy = CoreBusy::enter()?;
        let mut guard = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(core) = guard.as_mut() {
            core.pump();
            core.serve(request);
            return ack_rx.recv_timeout(timeout).ok();
        }
        drop(guard);
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        tx.as_ref()?.send(request).ok()?;
        drop(tx);
        ack_rx.recv_timeout(timeout).ok()
    }

    /// Advance CEF by one slice when the engine runs inline. A no-op when the
    /// engine owns a thread, which pumps itself.
    pub fn pump(&self) {
        let Some(_busy) = CoreBusy::enter() else {
            return;
        };
        if let Some(core) = self
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            core.pump();
        }
    }

    /// The view's current URL, as last reported by CEF's address change.
    /// Returns `None` before the first navigation.
    pub fn view_url(&self, view: ViewId) -> Option<String> {
        self.request(|ack| EngineRequest::Url { view, ack }, FRAME_TIMEOUT)
            .flatten()
    }

    /// Whether the view's history has an entry behind it.
    pub fn can_go_back(&self, view: ViewId) -> bool {
        self.history(view, true)
    }

    /// Whether the view's history has an entry ahead of it.
    pub fn can_go_forward(&self, view: ViewId) -> bool {
        self.history(view, false)
    }

    fn history(&self, view: ViewId, back: bool) -> bool {
        self.request(
            |ack| EngineRequest::History { view, back, ack },
            FRAME_TIMEOUT,
        )
        .unwrap_or(false)
    }

    /// Copy the view's last painted frame. Returns `None` before the first
    /// paint or once the view is gone.
    pub fn copy_frame(&self, view: ViewId) -> Option<Frame> {
        self.request(|ack| EngineRequest::Frame { view, ack }, FRAME_TIMEOUT)
            .flatten()
    }

    /// Forward one input event. Returns false if the view is unknown.
    pub fn send_input(&self, view: ViewId, event: InputEvent) -> bool {
        self.request(|ack| EngineRequest::Input { view, event, ack }, FRAME_TIMEOUT)
            .unwrap_or(false)
    }
}

impl Engine for CefEngine {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn pump(&self) {
        CefEngine::pump(self)
    }

    fn execute(&self, command: Command) -> Result<CommandOutcome, AdapterError> {
        command.validate()?;
        if !self.capabilities.supports(&command) {
            return Err(AdapterError::Unsupported(command.name()));
        }
        if matches!(command, Command::Shutdown) {
            return self.shutdown().map(|_| CommandOutcome::Done);
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(AdapterError::ShuttingDown);
        }
        self.send(command)
    }

    fn subscribe(&self) -> Option<EventStream> {
        self.rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(EventStream::new)
    }

    fn shutdown(&self) -> Result<(), AdapterError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.send(Command::Shutdown);
        if let Some(handle) = self
            .join
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        result.map(|_| ())
    }
}

impl Drop for CefEngine {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::SeqCst) {
            let _ = self.shutdown();
        }
    }
}

/// The live CEF session: everything one thread needs to drive CEF.
struct EngineCore {
    cef: Cef,
    sink: Arc<EventSink>,
    sink_ptr: *mut c_void,
    ids: IdAllocator,
    state: EngineState,
    /// Whether this core has to pump CEF itself; see
    /// [`CefEngineConfig::external_pump`].
    external_pump: bool,
    /// Whether this core pumps CEF itself; see [`CefEngineConfig::host_pumps`].
    host_pumps: bool,
    /// Whether a command may pump inline; see
    /// [`CefEngineConfig::pump_in_send`].
    pump_in_send: bool,
    /// Whether a timer window owns the pump; see
    /// [`CefEngineConfig::pump_timer`].
    pump_timer: bool,
}

// SAFETY: a core is only ever touched by the thread that initialized CEF —
// either the dedicated engine thread or the caller's own thread.
unsafe impl Send for EngineCore {}

impl EngineCore {
    fn start(config: &CefEngineConfig, event_tx: Sender<EngineMessage>) -> Result<Self, CefError> {
        let cef = Cef::init_with_args_mode_ex(
            &config.resources_dir,
            &config.locales_dir,
            &config.cache_dir,
            &config.extra_args,
            config.windowless,
            config.external_pump,
        )?;

        let sink = Arc::new(EventSink::new(event_tx));
        let sink_ptr = Arc::into_raw(Arc::clone(&sink)) as *mut c_void;
        // SAFETY: the pointer stays valid until the matching `from_raw` in
        // `finish`, which happens after CEF has shut down and stopped calling
        // back.
        unsafe { ffi::cinder_cef_set_event_callback(Some(on_cef_event), sink_ptr) };

        sink.emit(
            None,
            Event::EngineInitialized {
                engine_version: Cef::version(),
            },
        );
        let pump_timer = config.pump_timer && cef.start_pump_timer(PUMP_MS);
        Ok(Self {
            cef,
            sink,
            sink_ptr,
            ids: IdAllocator::new(),
            state: EngineState::default(),
            external_pump: config.external_pump,
            pump_in_send: config.pump_in_send,
            pump_timer,
            host_pumps: config.host_pumps,
        })
    }

    fn pump(&mut self) {
        if self.external_pump && self.host_pumps && !self.pump_timer {
            trace("engine pump cef enter");
            self.cef.pump(PUMP_MS);
            trace("engine pump cef done");
        }
        reconcile(&mut self.state, &self.sink);
        trace("engine pump reconciled");
    }

    /// Handle one request. Returns false once the session has shut down.
    fn serve(&mut self, request: EngineRequest) -> bool {
        match request {
            EngineRequest::Command { command, ack } => {
                if matches!(command, Command::Shutdown) {
                    let result =
                        shutdown_views(&self.cef, &mut self.state, &self.sink, self.pump_in_send);
                    let _ = ack.send(result);
                    return false;
                }
                let result = handle(
                    &self.cef,
                    &mut self.state,
                    &self.sink,
                    &self.ids,
                    command,
                    self.pump_in_send,
                );
                let _ = ack.send(result);
            }
            EngineRequest::Frame { view, ack } => {
                let _ = ack.send(read_frame(view));
            }
            EngineRequest::Input { view, event, ack } => {
                let _ = ack.send(apply_input(view, event));
            }
            EngineRequest::Url { view, ack } => {
                let _ = ack.send(read_url(view));
            }
            EngineRequest::History { view, back, ack } => {
                let _ = ack.send(read_history(view, back));
            }
        }
        true
    }

    fn finish(self) {
        finish(&self.cef, &self.sink, self.sink_ptr);
    }
}

fn run_engine(
    config: CefEngineConfig,
    cmd_rx: Receiver<EngineRequest>,
    event_tx: Sender<EngineMessage>,
    init_tx: Sender<Result<(), String>>,
) {
    let mut core = match EngineCore::start(&config, event_tx) {
        Ok(core) => core,
        Err(e) => {
            let _ = init_tx.send(Err(e.to_string()));
            return;
        }
    };
    let _ = init_tx.send(Ok(()));

    loop {
        core.pump();
        while let Ok(request) = cmd_rx.try_recv() {
            if !core.serve(request) {
                core.finish();
                return;
            }
        }
    }
}

/// The view's current URL, as last reported by CEF's address change.
fn read_url(view: ViewId) -> Option<String> {
    // SAFETY: the shim returns a pointer to its own NUL-terminated buffer.
    let ptr = unsafe { ffi::cinder_cef_view_url(view.get() as c_int) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer is valid until the next shim call on this thread.
    let text = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    (!text.is_empty()).then_some(text)
}

/// Whether history can move back (`back`) or forward.
fn read_history(view: ViewId, back: bool) -> bool {
    let raw = view.get() as c_int;
    // SAFETY: plain integer calls into the shim.
    unsafe {
        if back {
            ffi::cinder_cef_can_go_back(raw) != 0
        } else {
            ffi::cinder_cef_can_go_forward(raw) != 0
        }
    }
}

/// Copy the view's last painted frame out of the shim. Runs on the engine
/// thread, which is the only thread allowed to touch the shim.
fn read_frame(view: ViewId) -> Option<Frame> {
    let raw = view.get() as c_int;
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    // SAFETY: reads two ints the shim owns; no pointers escape.
    let ok = unsafe { ffi::cinder_cef_view_frame_size(raw, &mut width, &mut height) };
    if ok == 0 || width <= 0 || height <= 0 {
        return None;
    }
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: the buffer is exactly the size the shim just reported.
    let written = unsafe {
        ffi::cinder_cef_copy_view_frame(raw, bgra.as_mut_ptr(), bgra.len() as c_int)
    };
    if written <= 0 {
        return None;
    }
    bgra.truncate(written as usize);
    Some(Frame {
        width: width as u32,
        height: height as u32,
        bgra,
    })
}

/// Replay one shell input event into the page. Runs on the engine thread.
fn apply_input(view: ViewId, event: InputEvent) -> bool {
    let raw = view.get() as c_int;
    // SAFETY: every call passes plain integers; the shim owns the CEF objects.
    unsafe {
        match event {
            InputEvent::MouseMove {
                x,
                y,
                modifiers,
                leave,
            } => ffi::cinder_cef_send_mouse_move(
                raw,
                x,
                y,
                modifiers,
                i32::from(leave),
            ),
            InputEvent::MouseClick {
                x,
                y,
                modifiers,
                button,
                up,
                click_count,
            } => ffi::cinder_cef_send_mouse_click(
                raw,
                x,
                y,
                modifiers,
                button as c_int,
                i32::from(up),
                click_count,
            ),
            InputEvent::MouseWheel {
                x,
                y,
                modifiers,
                delta_x,
                delta_y,
            } => ffi::cinder_cef_send_mouse_wheel(raw, x, y, modifiers, delta_x, delta_y),
            InputEvent::Key {
                kind,
                modifiers,
                windows_key_code,
                native_key_code,
                is_system_key,
                character,
                unmodified_character,
            } => ffi::cinder_cef_send_key(
                raw,
                kind as c_int,
                modifiers,
                windows_key_code,
                native_key_code,
                i32::from(is_system_key),
                character,
                unmodified_character,
            ),
        }
    }
    true
}

fn finish(cef: &Cef, sink: &EventSink, sink_ptr: *mut c_void) {    sink.emit(None, Event::ShutdownComplete);
    // SAFETY: unregister before releasing the sink so no callback can observe a
    // dangling pointer.
    unsafe {
        ffi::cinder_cef_set_event_callback(None, std::ptr::null_mut());
        drop(Arc::from_raw(sink_ptr as *const EventSink));
    }
    let _ = sink;
    let _ = cef;
}

fn reconcile(state: &mut EngineState, sink: &EventSink) {
    for (view, view_state) in state.views.iter_mut() {
        if view_state.is_terminal() {
            continue;
        }
        // SAFETY: reads an int from the shim on the engine thread.
        let raw = unsafe { ffi::cinder_cef_view_state(view.get() as c_int) };
        if raw & STATE_CLOSED != 0 {
            *view_state = ViewState::Closed;
            sink.unbind(view.get() as i32);
        } else if raw & STATE_CREATED != 0 && matches!(view_state, ViewState::Creating) {
            *view_state = ViewState::Live;
        }
    }
}

fn shutdown_views(
    cef: &Cef,
    state: &mut EngineState,
    sink: &EventSink,
    inline_pump: bool,
) -> Result<CommandOutcome, AdapterError> {
    let open: Vec<ViewId> = state
        .views
        .iter()
        .filter(|(_, s)| !s.is_terminal())
        .map(|(v, _)| *v)
        .collect();

    for view in &open {
        sink.emit(Some(*view), Event::ViewClosing { view: *view });
        state.views.insert(*view, ViewState::Closing);
        // SAFETY: engine thread, valid view id.
        unsafe { ffi::cinder_cef_close_view(view.get() as c_int, 1) };
    }

    let deadline = Instant::now() + CLOSE_TIMEOUT;
    while Instant::now() < deadline {
        if inline_pump {
            cef.pump(PUMP_MS);
        }
        reconcile(state, sink);
        if state.views.values().all(|s| s.is_terminal()) {
            break;
        }
    }

    state.closed = true;
    Ok(CommandOutcome::Done)
}

fn wait_for_created(cef: &Cef, raw: i32) -> bool {
    let deadline = Instant::now() + CREATE_TIMEOUT;
    loop {
        // SAFETY: engine thread, plain int read.
        let state = unsafe { ffi::cinder_cef_view_state(raw) };
        if state & STATE_CREATED != 0 {
            return true;
        }
        if state & STATE_CLOSED != 0 || Instant::now() >= deadline {
            return false;
        }
        cef.pump(PUMP_MS);
    }
}

fn handle(
    cef: &Cef,
    state: &mut EngineState,
    sink: &EventSink,
    ids: &IdAllocator,
    command: Command,
    inline_pump: bool,
) -> Result<CommandOutcome, AdapterError> {
    match command {
        Command::Shutdown => Ok(CommandOutcome::Done),

        Command::CreateProfile { path, incognito } => {
            if path.is_empty() {
                return Err(AdapterError::InvalidArgument("empty profile path"));
            }
            let id = ids.next_profile();
            state.profiles.insert(id, incognito);
            Ok(CommandOutcome::ProfileCreated(id))
        }

        Command::CloseProfile { profile } => {
            if state.profiles.remove(&profile).is_none() {
                return Err(AdapterError::ProfileClosed);
            }
            Ok(CommandOutcome::Done)
        }

        Command::ClearData { profile, .. } => {
            if !state.profiles.contains_key(&profile) {
                return Err(AdapterError::ProfileClosed);
            }
            // Chromium owns the storage; deleting it needs the cookie manager's
            // async completion path, which is not wired up yet.
            Err(AdapterError::Unsupported("clear_data"))
        }

        Command::CreateView {
            profile,
            surface,
            initial_url,
        } => {
            if !state.profiles.contains_key(&profile) {
                return Err(AdapterError::ProfileClosed);
            }
            let (parent, top) = match surface {
                Surface::NativeChild { parent, top, .. } => (parent, top),
                Surface::Offscreen { .. } => (0, 0),
            };
            let (width, height) = surface.size();
            let url = initial_url
                .as_ref()
                .map(|u| u.as_str().to_string())
                .unwrap_or_else(|| "about:blank".to_string());
            let url = c_string(&url, "url")?;

            let view = ids.next_view();
            let raw = view.get() as c_int;
            // SAFETY: engine thread; url is NUL-terminated; dimensions validated.
            let ok = unsafe {
                ffi::cinder_cef_create_view(
                    raw,
                    url.as_ptr(),
                    width as c_int,
                    height as c_int,
                    parent,
                    top as c_int,
                )
            };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }

            state.views.insert(view, ViewState::Creating);
            sink.bind(raw, view);
            // Waiting means pumping, and pumping runs a nested run loop on
            // macOS, which re-enters the host's event handler. Where the host
            // does not pump, the view is reported as created straight away and
            // the run loop timer promotes it once CEF reports the browser.
            if !inline_pump {
                return Ok(CommandOutcome::ViewCreated(view));
            }
            if !wait_for_created(cef, raw) {
                state.views.insert(view, ViewState::Failed);
                sink.unbind(raw);
                return Err(AdapterError::EngineFault(
                    "CEF did not report browser creation".into(),
                ));
            }
            state.views.insert(view, ViewState::Live);
            Ok(CommandOutcome::ViewCreated(view))
        }

        Command::CloseView { view, force } => {
            let current = state.views.get(&view).copied().ok_or(AdapterError::ViewClosed)?;
            if current.is_terminal() {
                return Err(AdapterError::ViewClosed);
            }
            sink.emit(Some(view), Event::ViewClosing { view });
            state.views.insert(view, ViewState::Closing);
            // SAFETY: engine thread, valid view id.
            let ok = unsafe { ffi::cinder_cef_close_view(view.get() as c_int, force as c_int) };
            if ok != 1 && force {
                state.views.insert(view, ViewState::Closed);
                sink.emit(Some(view), Event::ViewClosed { view });
                sink.unbind(view.get() as c_int);
            }
            Ok(CommandOutcome::Done)
        }

        Command::Navigate { view, url } => {
            state.gate(view)?;
            let url = c_string(url.as_str(), "url")?;
            // SAFETY: engine thread, live view, NUL-terminated url.
            let ok = unsafe { ffi::cinder_cef_navigate(view.get() as c_int, url.as_ptr()) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::Reload { view, .. } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok = unsafe { ffi::cinder_cef_reload(view.get() as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::Stop { view } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok = unsafe { ffi::cinder_cef_stop(view.get() as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::GoBack { view } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok = unsafe { ffi::cinder_cef_go_back(view.get() as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::GoForward { view } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok = unsafe { ffi::cinder_cef_go_forward(view.get() as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::SetZoom { view, factor } => {
            state.gate(view)?;
            // CEF zoom levels are logarithmic in 1.2, the Chromium convention.
            let level = (factor as f64).ln() / 1.2f64.ln();
            // SAFETY: engine thread, live view, finite level.
            let ok = unsafe { ffi::cinder_cef_set_zoom(view.get() as c_int, level) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::MuteAudio { view, muted } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok = unsafe { ffi::cinder_cef_set_mute(view.get() as c_int, muted as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::FindInPage {
            view,
            query,
            forward,
        } => {
            state.gate(view)?;
            let query = c_string(&query, "find query")?;
            // SAFETY: engine thread, live view, NUL-terminated query.
            let ok = unsafe {
                ffi::cinder_cef_find(view.get() as c_int, query.as_ptr(), forward as c_int)
            };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::Resize {
            view,
            width,
            height,
            scale: _,
        } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view, validated dimensions.
            let ok = unsafe {
                ffi::cinder_cef_resize(view.get() as c_int, width as c_int, height as c_int)
            };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::SetVisible { view, visible } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok =
                unsafe { ffi::cinder_cef_set_visible(view.get() as c_int, visible as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::SetFocus { view, focused } => {
            state.gate(view)?;
            // SAFETY: engine thread, live view.
            let ok =
                unsafe { ffi::cinder_cef_set_focus(view.get() as c_int, focused as c_int) };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            Ok(CommandOutcome::Done)
        }

        Command::InstallPolicy { snapshot } => {
            let hosts: Vec<CString> = snapshot
                .blocked_hosts()
                .iter()
                .map(|h| c_string(h, "blocked host"))
                .collect::<Result<_, _>>()?;
            let ptrs: Vec<*const c_char> = hosts.iter().map(|h| h.as_ptr()).collect();
            // SAFETY: engine thread; the pointer array and every string outlive
            // the call, and the shim copies them.
            let ok = unsafe {
                ffi::cinder_cef_set_policy(
                    ptrs.as_ptr(),
                    ptrs.len() as c_int,
                    snapshot.https_upgrade() as c_int,
                )
            };
            if ok != 1 {
                return Err(AdapterError::EngineFault(Cef::last_error()));
            }
            state.policy = Some(snapshot);
            Ok(CommandOutcome::Done)
        }
    }
}
