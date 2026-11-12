//! The closed set of adapter errors (spec §9). No raw engine codes leak out.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    /// The engine has not finished initializing.
    NotInitialized,
    /// Shutdown has begun; new work is rejected.
    ShuttingDown,
    /// The view is closing; navigation-style commands are rejected.
    ViewClosing,
    /// The view is gone; its ID is retired.
    ViewClosed,
    /// The profile context is gone.
    ProfileClosed,
    /// The engine or this build does not provide the capability.
    Unsupported(&'static str),
    /// Caller supplied something the adapter refuses (bad size, bad factor...).
    InvalidArgument(&'static str),
    /// An adapter invariant was breached or the engine reported a fault.
    EngineFault(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => f.write_str("engine not initialized"),
            Self::ShuttingDown => f.write_str("engine is shutting down"),
            Self::ViewClosing => f.write_str("view is closing"),
            Self::ViewClosed => f.write_str("view is closed"),
            Self::ProfileClosed => f.write_str("profile context is closed"),
            Self::Unsupported(what) => write!(f, "unsupported by this engine: {what}"),
            Self::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Self::EngineFault(msg) => write!(f, "engine fault: {msg}"),
        }
    }
}

impl std::error::Error for AdapterError {}
