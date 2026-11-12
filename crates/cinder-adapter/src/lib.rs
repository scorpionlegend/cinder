//! Cinder engine adapter — layer L2 of the adapter specification.
//!
//! This crate is the *only* interface product code (L3) sees for web-engine
//! work. It contains no native code and no `unsafe`: the CEF-backed
//! implementation (L1/L0) lives in a separate crate and must satisfy these
//! traits. See `Cinder_Engine_Adapter_Interface_Spec.md`.
//!
//! Deliberate deviation from the spec text: decision requests are delivered on
//! the same stream as events but as a distinct [`EngineMessage::Decision`]
//! variant. A decision carries a one-shot responder, so it must not be
//! cloneable or silently buffered the way plain events are; keeping it out of
//! [`Event`] makes that impossible to get wrong.
#![forbid(unsafe_code)]

pub mod capability;
pub mod command;
pub mod decision;
pub mod engine;
pub mod error;
pub mod event;
pub mod ids;
pub mod policy;
pub mod sanitize;
pub mod view_state;

pub use capability::Capabilities;
pub use command::{Command, CommandOutcome, DataCategories, Surface};
pub use decision::{
    Decision, DecisionKind, DecisionRequest, DialogKind, NewWindowDisposition, PermissionKind,
    Responder,
};
pub use engine::Engine;
pub use error::AdapterError;
pub use event::{
    CrashCause, EngineMessage, Event, EventEnvelope, EventStream, LoadFailure, SecurityState,
};
pub use ids::{DecisionId, IdAllocator, ProfileId, SeqNo, ViewId};
pub use policy::{PolicySnapshot, RequestDecision};
pub use sanitize::{sanitize_filename, sanitize_title, OmniboxInput, Url, UrlError};
pub use view_state::{CommandGate, ViewState};
