//! A scripted, in-memory engine that implements the L2 traits.
//!
//! It exists so product code can be built and tested without native code
//! (spec §11). It can script permission requests, renderer crashes, views that
//! refuse to close, and dropped decision requests.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinder_adapter::capability::Capabilities;
use cinder_adapter::command::{Command, CommandOutcome};
use cinder_adapter::decision::{
    Decision, DecisionId, DecisionKind, DecisionRequest, PermissionKind, Responder,
};
use cinder_adapter::engine::Engine;
use cinder_adapter::error::AdapterError;
use cinder_adapter::event::{
    CrashCause, EngineMessage, Event, EventEnvelope, EventStream, LoadFailure,
};
use cinder_adapter::ids::{IdAllocator, ProfileId, SeqNo, ViewId};
use cinder_adapter::policy::PolicySnapshot;
use cinder_adapter::sanitize::{sanitize_title, Url};
use cinder_adapter::view_state::ViewState;

/// How long a scripted decision request stays open.
pub const DEFAULT_DECISION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct MockConfig {
    pub capabilities: Capabilities,
    /// Views emit `ViewClosing` but never `ViewClosed` unless force-closed.
    pub refuse_close: bool,
    /// Ask for this permission after each successful load.
    pub permission_on_load: Option<PermissionKind>,
    /// Terminate the renderer right after a load finishes.
    pub crash_after_load: bool,
    /// Drop decision requests instead of delivering them (tests the safe
    /// default path).
    pub drop_decision_requests: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            capabilities: Capabilities::cef_baseline_expected(),
            refuse_close: false,
            permission_on_load: None,
            crash_after_load: false,
            drop_decision_requests: false,
        }
    }
}

#[derive(Debug, Default)]
struct MockState {
    profiles: HashMap<ProfileId, bool>,
    views: HashMap<ViewId, ViewState>,
    policy: Option<Arc<PolicySnapshot>>,
    decisions: Vec<(DecisionId, Decision)>,
    closed: bool,
    shutdown_started: bool,
}

struct Inner {
    config: MockConfig,
    tx: Sender<EngineMessage>,
    rx: Mutex<Option<Receiver<EngineMessage>>>,
    ids: IdAllocator,
    state: Arc<Mutex<MockState>>,
    seq: Mutex<HashMap<ViewId, SeqNo>>,
    shutdown_complete: AtomicBool,
}

impl Inner {
    fn emit(&self, view: Option<ViewId>, event: Event) {
        let seq = match view {
            Some(v) => {
                let mut map = self.seq.lock().expect("seq lock");
                let entry = map.entry(v).or_default();
                let current = *entry;
                *entry = current.next();
                current
            }
            None => SeqNo::default(),
        };
        if matches!(event, Event::ShutdownComplete) {
            self.shutdown_complete.store(true, Ordering::SeqCst);
        }
        let _ = self.tx.send(EngineMessage::Event(EventEnvelope::new(seq, event)));
    }

    fn view_state(&self, view: ViewId) -> Result<ViewState, AdapterError> {
        self.state
            .lock()
            .expect("state lock")
            .views
            .get(&view)
            .copied()
            .ok_or(AdapterError::ViewClosed)
    }

    fn set_view_state(&self, view: ViewId, state: ViewState) {
        self.state
            .lock()
            .expect("state lock")
            .views
            .insert(view, state);
    }

    fn gate(&self, view: ViewId) -> Result<ViewState, AdapterError> {
        let state = self.view_state(view)?;
        match state {
            ViewState::Live => Ok(state),
            ViewState::Creating => Err(AdapterError::NotInitialized),
            ViewState::Closing => Err(AdapterError::ViewClosing),
            ViewState::Closed => Err(AdapterError::ViewClosed),
            ViewState::Failed => Err(AdapterError::EngineFault("view failed".into())),
        }
    }

    fn navigate(&self, view: ViewId, url: &Url) {
        self.emit(Some(view), Event::NavigationStarted { view, url: url.clone() });
        self.emit(Some(view), Event::UrlChanged { view, url: url.clone() });
        let title = match url.host() {
            Some(host) => sanitize_title(&format!("{host} — mock page")),
            None => sanitize_title(url.as_str()),
        };
        self.emit(Some(view), Event::TitleChanged { view, title });
        self.emit(Some(view), Event::LoadProgress { view, fraction: 0.5 });
        self.emit(Some(view), Event::NavigationCommitted { view, url: url.clone() });
        self.emit(Some(view), Event::LoadProgress { view, fraction: 1.0 });
        self.emit(Some(view), Event::LoadFinished { view });

        if let Some(kind) = self.config.permission_on_load {
            self.request_permission(view, url, kind);
        }
        if self.config.crash_after_load {
            self.emit(
                Some(view),
                Event::RendererTerminated {
                    view,
                    cause: CrashCause::OutOfMemory,
                },
            );
        }
    }

    fn request_permission(&self, view: ViewId, url: &Url, kind: PermissionKind) {
        let id = self.ids.next_decision();
        let origin = url.origin();
        let inner_state = Arc::clone(&self.state);
        let responder = Responder::new(Decision::SAFE_DEFAULT, move |decision| {
            inner_state
                .lock()
                .expect("state lock")
                .decisions
                .push((id, decision));
        });
        let request = DecisionRequest::new(
            id,
            view,
            DecisionKind::Permission { kind, origin },
            Instant::now() + DEFAULT_DECISION_TIMEOUT,
            responder,
        );
        if self.config.drop_decision_requests {
            // Dropping sends the safe default; the engine never waits.
            drop(request);
        } else {
            let _ = self.tx.send(EngineMessage::Decision(request));
        }
    }
}

/// Handle to the mock engine. Cheap to clone; all clones share one engine.
#[derive(Clone)]
pub struct MockEngine {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MockEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockEngine")
            .field("config", &self.inner.config)
            .finish()
    }
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MockEngine {
    pub fn new() -> Self {
        Self::with_config(MockConfig::default())
    }

    pub fn with_config(config: MockConfig) -> Self {
        let (tx, rx) = channel();
        Self {
            inner: Arc::new(Inner {
                config,
                tx,
                rx: Mutex::new(Some(rx)),
                ids: IdAllocator::new(),
                state: Arc::new(Mutex::new(MockState::default())),
                seq: Mutex::new(HashMap::new()),
                shutdown_complete: AtomicBool::new(false),
            }),
        }
    }

    pub fn builder() -> MockEngineBuilder {
        MockEngineBuilder::default()
    }

    /// Decisions the engine received, in order. Dropped requests appear with
    /// their safe default.
    pub fn decisions(&self) -> Vec<(DecisionId, Decision)> {
        self.inner
            .state
            .lock()
            .expect("state lock")
            .decisions
            .clone()
    }

    pub fn installed_policy_version(&self) -> Option<u64> {
        self.inner
            .state
            .lock()
            .expect("state lock")
            .policy
            .as_ref()
            .map(|p| p.version())
    }

    pub fn shutdown_complete(&self) -> bool {
        self.inner.shutdown_complete.load(Ordering::SeqCst)
    }
}

impl Engine for MockEngine {
    fn capabilities(&self) -> Capabilities {
        self.inner.config.capabilities
    }

    fn execute(&self, command: Command) -> Result<CommandOutcome, AdapterError> {
        command.validate()?;
        if !self.inner.config.capabilities.supports(&command) {
            return Err(AdapterError::Unsupported(command.name()));
        }
        if self.inner.state.lock().expect("state lock").closed {
            return Err(AdapterError::ShuttingDown);
        }

        match command {
            Command::Shutdown => self.shutdown().map(|_| CommandOutcome::Done),

            Command::CreateProfile { path, incognito } => {
                if path.is_empty() {
                    return Err(AdapterError::InvalidArgument("empty profile path"));
                }
                let id = self.inner.ids.next_profile();
                self.inner
                    .state
                    .lock()
                    .expect("state lock")
                    .profiles
                    .insert(id, incognito);
                Ok(CommandOutcome::ProfileCreated(id))
            }

            Command::CloseProfile { profile } => {
                let mut state = self.inner.state.lock().expect("state lock");
                if state.profiles.remove(&profile).is_none() {
                    return Err(AdapterError::ProfileClosed);
                }
                Ok(CommandOutcome::Done)
            }

            Command::ClearData { profile, .. } => {
                if !self
                    .inner
                    .state
                    .lock()
                    .expect("state lock")
                    .profiles
                    .contains_key(&profile)
                {
                    return Err(AdapterError::ProfileClosed);
                }
                Ok(CommandOutcome::Done)
            }

            Command::CreateView {
                profile,
                surface: _,
                initial_url,
            } => {
                if !self
                    .inner
                    .state
                    .lock()
                    .expect("state lock")
                    .profiles
                    .contains_key(&profile)
                {
                    return Err(AdapterError::ProfileClosed);
                }
                let view = self.inner.ids.next_view();
                self.inner.set_view_state(view, ViewState::Creating);
                self.inner.emit(Some(view), Event::ViewCreated { view });
                self.inner.set_view_state(view, ViewState::Live);
                self.inner.emit(Some(view), Event::ViewReady { view });
                if let Some(url) = initial_url {
                    self.inner.navigate(view, &url);
                }
                Ok(CommandOutcome::ViewCreated(view))
            }

            Command::CloseView { view, force } => {
                let state = self.inner.view_state(view)?;
                if state.is_terminal() {
                    return Err(AdapterError::ViewClosed);
                }
                self.inner.emit(Some(view), Event::ViewClosing { view });
                self.inner.set_view_state(view, ViewState::Closing);
                if force || !self.inner.config.refuse_close {
                    self.inner.emit(Some(view), Event::ViewClosed { view });
                    self.inner.set_view_state(view, ViewState::Closed);
                }
                Ok(CommandOutcome::Done)
            }

            Command::Navigate { view, url } => {
                self.inner.gate(view)?;
                self.inner.navigate(view, &url);
                Ok(CommandOutcome::Done)
            }

            Command::Reload { view, .. } => {
                self.inner.gate(view)?;
                self.inner.emit(Some(view), Event::LoadFinished { view });
                Ok(CommandOutcome::Done)
            }

            Command::Stop { view } => {
                self.inner.gate(view)?;
                self.inner.emit(
                    Some(view),
                    Event::LoadFailed {
                        view,
                        reason: LoadFailure::Aborted,
                    },
                );
                Ok(CommandOutcome::Done)
            }

            Command::GoBack { view } | Command::GoForward { view } => {
                self.inner.gate(view)?;
                Ok(CommandOutcome::Done)
            }

            Command::SetVisible { view, .. }
            | Command::SetFocus { view, .. }
            | Command::Resize { view, .. }
            | Command::SetZoom { view, .. }
            | Command::MuteAudio { view, .. }
            | Command::FindInPage { view, .. } => {
                self.inner.gate(view)?;
                Ok(CommandOutcome::Done)
            }

            Command::InstallPolicy { snapshot } => {
                self.inner.state.lock().expect("state lock").policy = Some(snapshot);
                Ok(CommandOutcome::Done)
            }
        }
    }

    fn subscribe(&self) -> Option<EventStream> {
        self.inner
            .rx
            .lock()
            .expect("rx lock")
            .take()
            .map(EventStream::new)
    }

    fn shutdown(&self) -> Result<(), AdapterError> {
        let mut state = self.inner.state.lock().expect("state lock");
        if state.closed {
            return Ok(());
        }
        state.shutdown_started = true;
        let views: Vec<(ViewId, ViewState)> = state.views.iter().map(|(v, s)| (*v, *s)).collect();
        drop(state);

        for (view, view_state) in &views {
            if *view_state == ViewState::Live {
                self.inner.emit(Some(*view), Event::ViewClosing { view: *view });
                self.inner.set_view_state(*view, ViewState::Closing);
            }
        }

        let still_open = {
            let state = self.inner.state.lock().expect("state lock");
            state
                .views
                .values()
                .any(|s| !s.is_terminal())
        };

        if still_open {
            return Err(AdapterError::EngineFault(
                "views did not confirm closure before shutdown".into(),
            ));
        }

        self.inner.emit(None, Event::ShutdownComplete);
        self.inner.state.lock().expect("state lock").closed = true;
        Ok(())
    }
}

/// Builder for scripted scenarios.
#[derive(Debug, Default)]
pub struct MockEngineBuilder {
    config: MockConfig,
}

impl MockEngineBuilder {
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.config.capabilities = caps;
        self
    }

    pub fn refuse_close(mut self, refuse: bool) -> Self {
        self.config.refuse_close = refuse;
        self
    }

    pub fn request_permission(mut self, kind: PermissionKind) -> Self {
        self.config.permission_on_load = Some(kind);
        self
    }

    pub fn crash_after_load(mut self, crash: bool) -> Self {
        self.config.crash_after_load = crash;
        self
    }

    pub fn drop_decision_requests(mut self, drop: bool) -> Self {
        self.config.drop_decision_requests = drop;
        self
    }

    pub fn build(self) -> MockEngine {
        MockEngine::with_config(self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinder_adapter::command::Surface;

    fn engine_with_profile() -> (MockEngine, ProfileId) {
        let engine = MockEngine::new();
        let profile = match engine
            .execute(Command::CreateProfile {
                path: "/tmp/cinder-test".into(),
                incognito: false,
            })
            .unwrap()
        {
            CommandOutcome::ProfileCreated(p) => p,
            other => panic!("unexpected outcome {other:?}"),
        };
        (engine, profile)
    }

    #[test]
    fn view_lifecycle_emits_created_ready_closed() {
        let (engine, profile) = engine_with_profile();
        let stream = engine.subscribe().unwrap();
        let view = match engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::Offscreen {
                    width: 800,
                    height: 600,
                },
                initial_url: None,
            })
            .unwrap()
        {
            CommandOutcome::ViewCreated(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        engine
            .execute(Command::CloseView { view, force: false })
            .unwrap();
        let names: Vec<&str> = stream
            .drain()
            .iter()
            .filter_map(|m| m.event().map(|e| e.event.name()))
            .collect();
        assert_eq!(
            names,
            vec!["view_created", "view_ready", "view_closing", "view_closed"]
        );
    }

    #[test]
    fn navigation_emits_ordered_events_with_increasing_seq() {
        let (engine, profile) = engine_with_profile();
        let stream = engine.subscribe().unwrap();
        let view = match engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::Offscreen {
                    width: 800,
                    height: 600,
                },
                initial_url: None,
            })
            .unwrap()
        {
            CommandOutcome::ViewCreated(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let _ = stream.drain();
        engine
            .execute(Command::Navigate {
                view,
                url: Url::parse("https://example.com/").unwrap(),
            })
            .unwrap();
        let seqs: Vec<u64> = stream
            .drain()
            .iter()
            .filter_map(|m| m.event().map(|e| e.seq.get()))
            .collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq must increase: {seqs:?}");
    }

    #[test]
    fn commands_on_closed_view_are_rejected() {
        let (engine, profile) = engine_with_profile();
        let view = match engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::Offscreen {
                    width: 800,
                    height: 600,
                },
                initial_url: None,
            })
            .unwrap()
        {
            CommandOutcome::ViewCreated(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        engine
            .execute(Command::CloseView { view, force: true })
            .unwrap();
        assert_eq!(
            engine.execute(Command::Stop { view }),
            Err(AdapterError::ViewClosed)
        );
    }

    #[test]
    fn dropped_decision_requests_record_the_safe_default() {
        let engine = MockEngine::builder()
            .request_permission(PermissionKind::Geolocation)
            .drop_decision_requests(true)
            .build();
        let profile = match engine
            .execute(Command::CreateProfile {
                path: "/tmp/p".into(),
                incognito: false,
            })
            .unwrap()
        {
            CommandOutcome::ProfileCreated(p) => p,
            other => panic!("unexpected {other:?}"),
        };
        let view = match engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::Offscreen {
                    width: 800,
                    height: 600,
                },
                initial_url: Some(Url::parse("https://maps.example/").unwrap()),
            })
            .unwrap()
        {
            CommandOutcome::ViewCreated(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let _ = view;
        assert_eq!(engine.decisions(), vec![(engine.decisions()[0].0, Decision::Deny)]);
    }

    #[test]
    fn shutdown_fails_while_a_view_refuses_to_close() {
        let engine = MockEngine::builder().refuse_close(true).build();
        let profile = match engine
            .execute(Command::CreateProfile {
                path: "/tmp/p".into(),
                incognito: false,
            })
            .unwrap()
        {
            CommandOutcome::ProfileCreated(p) => p,
            other => panic!("unexpected {other:?}"),
        };
        let view = match engine
            .execute(Command::CreateView {
                profile,
                surface: Surface::Offscreen {
                    width: 800,
                    height: 600,
                },
                initial_url: None,
            })
            .unwrap()
        {
            CommandOutcome::ViewCreated(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        assert!(engine.shutdown().is_err());
        assert!(!engine.shutdown_complete());
        engine
            .execute(Command::CloseView { view, force: true })
            .unwrap();
        engine.shutdown().unwrap();
        assert!(engine.shutdown_complete());
    }
}
