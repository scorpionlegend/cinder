//! Cinder orchestrator (L3): the Cinder-side model of windows, tabs and
//! sessions, driven only through the engine adapter.
#![forbid(unsafe_code)]

pub mod browser;
pub mod session;
pub mod tab;

pub use browser::{Browser, Effect, HistoryEntry, NavigationOutcome, ShutdownReport};
pub use session::{SessionError, SessionSnapshot, TabSnapshot, WindowSnapshot};
pub use tab::{Tab, TabId, Window, WindowId};
