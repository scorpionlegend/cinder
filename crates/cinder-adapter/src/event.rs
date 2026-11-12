//! Events: engine → Cinder (spec §5). Plain owned data only — no engine
//! handles, no borrowed engine memory.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::decision::DecisionRequest;
use crate::ids::{SeqNo, ViewId};
use crate::sanitize::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadFailure {
    Aborted,
    TimedOut,
    ConnectionFailed,
    CertificateError,
    BlockedByPolicy,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashCause {
    OutOfMemory,
    Killed,
    AbnormalExit,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityState {
    Unknown,
    Secure,
    Insecure,
    CertificateError,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    EngineInitialized {
        engine_version: String,
    },

    // View lifecycle
    ViewCreated {
        view: ViewId,
    },
    ViewReady {
        view: ViewId,
    },
    CloseRequested {
        view: ViewId,
    },
    ViewClosing {
        view: ViewId,
    },
    ViewClosed {
        view: ViewId,
    },

    // Navigation and loading
    NavigationStarted {
        view: ViewId,
        url: Url,
    },
    NavigationCommitted {
        view: ViewId,
        url: Url,
    },
    LoadProgress {
        view: ViewId,
        fraction: f32,
    },
    LoadFinished {
        view: ViewId,
    },
    LoadFailed {
        view: ViewId,
        reason: LoadFailure,
    },
    RedirectObserved {
        view: ViewId,
        from: Url,
        to: Url,
    },

    // Page metadata
    TitleChanged {
        view: ViewId,
        title: String,
    },
    FaviconChanged {
        view: ViewId,
        bytes: Vec<u8>,
    },
    UrlChanged {
        view: ViewId,
        url: Url,
    },
    SecurityStateChanged {
        view: ViewId,
        state: SecurityState,
    },
    AudioStateChanged {
        view: ViewId,
        audible: bool,
    },

    // Failures
    RendererTerminated {
        view: ViewId,
        cause: CrashCause,
    },
    ViewUnresponsive {
        view: ViewId,
    },
    GpuProcessFailed,
    EngineFatal {
        message: String,
    },
    AdapterFault {
        message: String,
    },

    // Engine lifecycle
    ShutdownComplete,

    /// Diagnostics: events for a closed/unknown view were dropped.
    EventsDropped {
        view: Option<ViewId>,
        count: u64,
    },
}

impl Event {
    pub fn view(&self) -> Option<ViewId> {
        match self {
            Self::ViewCreated { view }
            | Self::ViewReady { view }
            | Self::CloseRequested { view }
            | Self::ViewClosing { view }
            | Self::ViewClosed { view }
            | Self::NavigationStarted { view, .. }
            | Self::NavigationCommitted { view, .. }
            | Self::LoadProgress { view, .. }
            | Self::LoadFinished { view }
            | Self::LoadFailed { view, .. }
            | Self::RedirectObserved { view, .. }
            | Self::TitleChanged { view, .. }
            | Self::FaviconChanged { view, .. }
            | Self::UrlChanged { view, .. }
            | Self::SecurityStateChanged { view, .. }
            | Self::AudioStateChanged { view, .. }
            | Self::RendererTerminated { view, .. }
            | Self::ViewUnresponsive { view } => Some(*view),
            Self::EventsDropped { view, .. } => *view,
            Self::EngineInitialized { .. }
            | Self::GpuProcessFailed
            | Self::EngineFatal { .. }
            | Self::AdapterFault { .. }
            | Self::ShutdownComplete => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::EngineInitialized { .. } => "engine_initialized",
            Self::ViewCreated { .. } => "view_created",
            Self::ViewReady { .. } => "view_ready",
            Self::CloseRequested { .. } => "close_requested",
            Self::ViewClosing { .. } => "view_closing",
            Self::ViewClosed { .. } => "view_closed",
            Self::NavigationStarted { .. } => "navigation_started",
            Self::NavigationCommitted { .. } => "navigation_committed",
            Self::LoadProgress { .. } => "load_progress",
            Self::LoadFinished { .. } => "load_finished",
            Self::LoadFailed { .. } => "load_failed",
            Self::RedirectObserved { .. } => "redirect_observed",
            Self::TitleChanged { .. } => "title_changed",
            Self::FaviconChanged { .. } => "favicon_changed",
            Self::UrlChanged { .. } => "url_changed",
            Self::SecurityStateChanged { .. } => "security_state_changed",
            Self::AudioStateChanged { .. } => "audio_state_changed",
            Self::RendererTerminated { .. } => "renderer_terminated",
            Self::ViewUnresponsive { .. } => "view_unresponsive",
            Self::GpuProcessFailed => "gpu_process_failed",
            Self::EngineFatal { .. } => "engine_fatal",
            Self::AdapterFault { .. } => "adapter_fault",
            Self::ShutdownComplete => "shutdown_complete",
            Self::EventsDropped { .. } => "events_dropped",
        }
    }
}

/// An event plus its ordering sequence number.
#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    pub seq: SeqNo,
    pub event: Event,
}

impl EventEnvelope {
    pub fn new(seq: SeqNo, event: Event) -> Self {
        Self { seq, event }
    }

    pub fn view(&self) -> Option<ViewId> {
        self.event.view()
    }
}

/// One item from the engine. Decisions are separate from events because they
/// carry a one-shot responder.
#[derive(Debug)]
pub enum EngineMessage {
    Event(EventEnvelope),
    Decision(DecisionRequest),
}

impl EngineMessage {
    pub fn event(&self) -> Option<&EventEnvelope> {
        match self {
            Self::Event(e) => Some(e),
            Self::Decision(_) => None,
        }
    }

    pub fn decision(&self) -> Option<&DecisionRequest> {
        match self {
            Self::Event(_) => None,
            Self::Decision(d) => Some(d),
        }
    }
}

/// Bounded-by-consumer event stream. Product code owns this; the engine only
/// holds the sender.
#[derive(Debug)]
pub struct EventStream {
    rx: Receiver<EngineMessage>,
}

impl EventStream {
    pub fn new(rx: Receiver<EngineMessage>) -> Self {
        Self { rx }
    }

    /// Non-blocking poll.
    pub fn try_next(&self) -> Option<EngineMessage> {
        self.rx.try_recv().ok()
    }

    /// Drain everything currently queued.
    pub fn drain(&self) -> Vec<EngineMessage> {
        let mut out = Vec::new();
        while let Some(msg) = self.try_next() {
            out.push(msg);
        }
        out
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<EngineMessage, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}
