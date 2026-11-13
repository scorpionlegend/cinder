//! The engine trait (L2). Implemented by the mock engine and, later, by the
//! CEF-backed adapter.

use crate::capability::Capabilities;
use crate::command::{Command, CommandOutcome};
use crate::error::AdapterError;
use crate::event::EventStream;

/// One engine per process. `Send + Sync` so the orchestrator can hold it in an
/// `Arc`; thread affinity of individual calls is the implementation's problem
/// (spec §7).
pub trait Engine: Send + Sync {
    /// Capability set reported at initialization.
    fn capabilities(&self) -> Capabilities;

    /// Submit a command. Returns as soon as the command is accepted; effects
    /// arrive as events.
    fn execute(&self, command: Command) -> Result<CommandOutcome, AdapterError>;

    /// Advance the engine by one slice. Implementations that own a thread pump
    /// themselves and leave this as a no-op; ones that run on the caller's
    /// thread (CEF on macOS and Windows) need the caller's loop to call it.
    fn pump(&self) {}

    /// Take the single event stream. Implementations return `None` on the
    /// second call so two consumers cannot split the stream.
    fn subscribe(&self) -> Option<EventStream>;

    /// Begin orderly shutdown. `ShutdownComplete` is emitted when every view
    /// has confirmed closure.
    fn shutdown(&self) -> Result<(), AdapterError>;
}
