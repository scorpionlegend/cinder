//! The authoritative view state machine (spec §6). One copy, in L1; product
//! code reads state through the orchestrator, never by guessing.

use crate::command::Command;
use crate::error::AdapterError;
use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Creating,
    Live,
    Closing,
    Closed,
    Failed,
}

/// What to do with a command aimed at a view in a given state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandGate {
    Allow,
    /// Accepted but held until the view reaches `Live`.
    Queue,
    Reject(AdapterError),
}

impl ViewState {
    pub fn gate(&self, _command: &Command) -> CommandGate {
        match self {
            Self::Creating => CommandGate::Queue,
            Self::Live => CommandGate::Allow,
            Self::Closing => CommandGate::Reject(AdapterError::ViewClosing),
            Self::Closed => CommandGate::Reject(AdapterError::ViewClosed),
            Self::Failed => CommandGate::Reject(AdapterError::EngineFault("view failed to create".into())),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }

    /// Apply an engine event. Invalid transitions are an adapter invariant
    /// breach, not a user error.
    pub fn apply(&mut self, event: &Event) -> Result<(), AdapterError> {
        let next = match (&*self, event) {
            (Self::Creating, Event::ViewCreated { .. }) => Some(Self::Creating),
            (Self::Creating, Event::ViewReady { .. }) => Some(Self::Live),
            (Self::Creating, Event::ViewClosed { .. }) => Some(Self::Closed),
            (Self::Live, Event::CloseRequested { .. }) => Some(Self::Closing),
            (Self::Live, Event::ViewClosing { .. }) => Some(Self::Closing),
            (Self::Live, Event::ViewClosed { .. }) => Some(Self::Closed),
            (Self::Closing, Event::ViewClosing { .. }) => Some(Self::Closing),
            (Self::Closing, Event::ViewClosed { .. }) => Some(Self::Closed),
            _ => None,
        };

        match next {
            Some(state) => {
                *self = state;
                Ok(())
            }
            None => Err(AdapterError::EngineFault(format!(
                "invalid transition {:?} on {}",
                self,
                event.name()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ViewId;

    fn view() -> ViewId {
        ViewId::from_raw(1)
    }

    #[test]
    fn lifecycle_walks_creating_live_closing_closed() {
        let mut s = ViewState::Creating;
        s.apply(&Event::ViewCreated { view: view() }).unwrap();
        assert_eq!(s, ViewState::Creating);
        s.apply(&Event::ViewReady { view: view() }).unwrap();
        assert_eq!(s, ViewState::Live);
        s.apply(&Event::ViewClosing { view: view() }).unwrap();
        assert_eq!(s, ViewState::Closing);
        s.apply(&Event::ViewClosed { view: view() }).unwrap();
        assert_eq!(s, ViewState::Closed);
        assert!(s.is_terminal());
    }

    #[test]
    fn closing_rejects_navigation_but_closed_is_terminal() {
        let cmd = Command::Stop { view: view() };
        assert_eq!(
            ViewState::Closing.gate(&cmd),
            CommandGate::Reject(AdapterError::ViewClosing)
        );
        assert_eq!(
            ViewState::Closed.gate(&cmd),
            CommandGate::Reject(AdapterError::ViewClosed)
        );
        assert_eq!(ViewState::Creating.gate(&cmd), CommandGate::Queue);
        assert_eq!(ViewState::Live.gate(&cmd), CommandGate::Allow);
    }

    #[test]
    fn out_of_order_events_are_faults() {
        let mut s = ViewState::Live;
        assert!(s.apply(&Event::ViewReady { view: view() }).is_err());
        assert!(s.apply(&Event::ViewCreated { view: view() }).is_err());
        assert_eq!(s, ViewState::Live, "state must not change on a bad event");
    }
}
