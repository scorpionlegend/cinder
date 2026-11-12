//! Decision requests: events that need an answer (spec §6).
//!
//! A responder is one-shot and carries a safe default. Dropping it without
//! answering sends the default, so the engine can never wait forever.

use std::fmt;
use std::time::Instant;

pub use crate::ids::DecisionId;
use crate::ids::ViewId;
use crate::sanitize::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionKind {
    Camera,
    Microphone,
    Geolocation,
    Notifications,
    ClipboardRead,
    PersistentStorage,
}

impl PermissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Microphone => "microphone",
            Self::Geolocation => "geolocation",
            Self::Notifications => "notifications",
            Self::ClipboardRead => "clipboard-read",
            Self::PersistentStorage => "persistent-storage",
        }
    }
}

impl fmt::Display for PermissionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewWindowDisposition {
    ForegroundTab,
    BackgroundTab,
    NewWindow,
    Popup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    FileChooser,
    HttpAuth,
    CertificateError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionKind {
    Permission {
        kind: PermissionKind,
        origin: String,
    },
    NewWindow {
        url: Url,
        disposition: NewWindowDisposition,
        user_gesture: bool,
    },
    Download {
        url: Url,
        suggested_name: String,
    },
    Dialog {
        kind: DialogKind,
        message: String,
    },
    Fullscreen,
}

impl DecisionKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Permission { .. } => "permission",
            Self::NewWindow { .. } => "new_window",
            Self::Download { .. } => "download",
            Self::Dialog { .. } => "dialog",
            Self::Fullscreen => "fullscreen",
        }
    }
}

/// The answer. `AllowOnce` exists so a stored decision is not implied by a
/// single approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AllowOnce,
    Deny,
}

impl Decision {
    /// The safe default for every decision kind: deny / cancel.
    pub const SAFE_DEFAULT: Decision = Decision::Deny;

    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow | Self::AllowOnce)
    }
}

/// One-shot answer channel into the engine. Not cloneable on purpose.
pub struct Responder {
    inner: Option<Box<dyn FnOnce(Decision) + Send + 'static>>,
    default: Decision,
}

impl Responder {
    pub fn new(default: Decision, f: impl FnOnce(Decision) + Send + 'static) -> Self {
        Self {
            inner: Some(Box::new(f)),
            default,
        }
    }

    /// Answer the engine. Consumes the responder; the default is not sent.
    pub fn answer(mut self, decision: Decision) {
        if let Some(f) = self.inner.take() {
            f(decision);
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if let Some(f) = self.inner.take() {
            f(self.default);
        }
    }
}

impl fmt::Debug for Responder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Responder")
            .field("answered", &self.inner.is_none())
            .field("default", &self.default)
            .finish()
    }
}

#[derive(Debug)]
pub struct DecisionRequest {
    pub id: DecisionId,
    pub view: ViewId,
    pub kind: DecisionKind,
    pub deadline: Instant,
    responder: Responder,
}

impl DecisionRequest {
    pub fn new(
        id: DecisionId,
        view: ViewId,
        kind: DecisionKind,
        deadline: Instant,
        responder: Responder,
    ) -> Self {
        Self {
            id,
            view,
            kind,
            deadline,
            responder,
        }
    }

    /// Answer and consume the request.
    pub fn answer(self, decision: Decision) {
        self.responder.answer(decision);
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Drop without answering: the responder's `Drop` sends the safe default.
    pub fn abandon(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn dropping_a_responder_sends_the_safe_default() {
        let (tx, rx) = mpsc::channel();
        let responder = Responder::new(Decision::SAFE_DEFAULT, move |d| {
            tx.send(d).unwrap();
        });
        drop(responder);
        assert_eq!(rx.recv_timeout(Duration::from_millis(50)).unwrap(), Decision::Deny);
    }

    #[test]
    fn answering_sends_the_answer_not_the_default() {
        let (tx, rx) = mpsc::channel();
        let responder = Responder::new(Decision::SAFE_DEFAULT, move |d| {
            tx.send(d).unwrap();
        });
        responder.answer(Decision::AllowOnce);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            Decision::AllowOnce
        );
        assert!(rx.try_recv().is_err(), "responder must fire exactly once");
    }
}
