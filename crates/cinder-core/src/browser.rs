//! The orchestrator: owns the Cinder-side model and drives the engine through
//! the adapter only. No native types, no `unsafe`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cinder_adapter::command::{Command, CommandOutcome, Surface};
use cinder_adapter::decision::{Decision, DecisionId, DecisionKind, DecisionRequest, PermissionKind};
use cinder_adapter::engine::Engine;
use cinder_adapter::error::AdapterError;
use cinder_adapter::event::{EngineMessage, Event, LoadFailure};
use cinder_adapter::ids::{ProfileId, ViewId};
use cinder_adapter::policy::{PolicySnapshot, RequestDecision};
use cinder_adapter::sanitize::{sanitize_title, Url};

use crate::session::{SessionSnapshot, TabSnapshot, WindowSnapshot};
use crate::tab::{Tab, TabId, Window, WindowId};

/// Something the shell UI should show or do. Returned by [`Browser::pump`].
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    TabOpened(TabId),
    TabClosed(TabId),
    CloseRequested(TabId),
    TitleChanged { tab: TabId, title: String },
    UrlChanged { tab: TabId, url: String },
    LoadStarted { tab: TabId },
    LoadFinished { tab: TabId },
    LoadFailed { tab: TabId, reason: LoadFailure },
    RendererCrashed { tab: TabId },
    ViewUnresponsive { tab: TabId },
    NavigationBlocked { tab: TabId, url: String },
    PermissionPrompt {
        tab: TabId,
        kind: PermissionKind,
        origin: String,
    },
    DecisionRequired { tab: TabId, kind: &'static str },
    EngineFault { message: String },
    ShutdownComplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationOutcome {
    Started,
    Blocked,
    UpgradedToHttps(Url),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub url: Url,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Views that had to be force-closed after the graceful timeout.
    pub forced_closed: usize,
    /// Views still registered when shutdown gave up (should be 0).
    pub remaining: usize,
    pub shutdown_complete: bool,
}

pub struct Browser {
    engine: Arc<dyn Engine>,
    stream: cinder_adapter::event::EventStream,
    policy: Arc<PolicySnapshot>,
    profile: ProfileId,
    surface: Surface,
    tabs: BTreeMap<TabId, Tab>,
    by_view: HashMap<ViewId, TabId>,
    windows: BTreeMap<WindowId, Window>,
    active_window: Option<WindowId>,
    history: Vec<HistoryEntry>,
    pending: Vec<DecisionRequest>,
    next_tab: u64,
    next_window: u64,
    shutting_down: bool,
    shutdown_complete: bool,
    dropped_events: u64,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("profile", &self.profile)
            .field("tabs", &self.tabs.len())
            .field("windows", &self.windows.len())
            .field("pending_decisions", &self.pending.len())
            .field("shutting_down", &self.shutting_down)
            .finish()
    }
}

impl Browser {
    /// Create the orchestrator: subscribe to the engine, create the profile
    /// context, install the policy snapshot.
    pub fn new(
        engine: Arc<dyn Engine>,
        policy: Arc<PolicySnapshot>,
        profile_path: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        Self::with_surface(
            engine,
            policy,
            profile_path,
            Surface::Offscreen {
                width: 1280,
                height: 800,
            },
        )
    }

    pub fn with_surface(
        engine: Arc<dyn Engine>,
        policy: Arc<PolicySnapshot>,
        profile_path: impl Into<String>,
        surface: Surface,
    ) -> Result<Self, AdapterError> {
        let stream = engine
            .subscribe()
            .ok_or_else(|| AdapterError::EngineFault("event stream already taken".into()))?;

        let profile = match engine.execute(Command::CreateProfile {
            path: profile_path.into(),
            incognito: false,
        })? {
            CommandOutcome::ProfileCreated(p) => p,
            other => {
                return Err(AdapterError::EngineFault(format!(
                    "create_profile returned {other:?}"
                )))
            }
        };

        engine.execute(Command::InstallPolicy {
            snapshot: policy.clone(),
        })?;

        Ok(Self {
            engine,
            stream,
            policy,
            profile,
            surface,
            tabs: BTreeMap::new(),
            by_view: HashMap::new(),
            windows: BTreeMap::new(),
            active_window: None,
            history: Vec::new(),
            pending: Vec::new(),
            next_tab: 1,
            next_window: 1,
            shutting_down: false,
            shutdown_complete: false,
            dropped_events: 0,
        })
    }

    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    /// Replace the surface future tabs are created on. The shell learns its
    /// native window handle only after the window exists, so the surface is
    /// chosen before the first tab rather than at construction.
    pub fn set_surface(&mut self, surface: Surface) {
        self.surface = surface;
    }

    pub fn surface(&self) -> Surface {
        self.surface
    }

    pub fn policy(&self) -> &Arc<PolicySnapshot> {
        &self.policy
    }

    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::new(self.next_window);
        self.next_window += 1;
        self.windows.insert(id, Window::new(id));
        if self.active_window.is_none() {
            self.active_window = Some(id);
        }
        id
    }

    pub fn active_window(&self) -> Option<WindowId> {
        self.active_window
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.get(&id)
    }

    pub fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.values()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn pending_decisions(&self) -> &[DecisionRequest] {
        &self.pending
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    pub fn shutdown_complete(&self) -> bool {
        self.shutdown_complete
    }

    /// Open a tab in a window. The view is created through the adapter; the
    /// tab becomes visible once `ViewReady` arrives.
    pub fn open_tab(&mut self, window: WindowId, url: Option<Url>) -> Result<TabId, AdapterError> {
        if self.shutting_down {
            return Err(AdapterError::ShuttingDown);
        }
        if !self.windows.contains_key(&window) {
            return Err(AdapterError::InvalidArgument("unknown window"));
        }

        let view = match self.engine.execute(Command::CreateView {
            profile: self.profile,
            surface: self.surface,
            initial_url: url.clone(),
        })? {
            CommandOutcome::ViewCreated(v) => v,
            other => {
                return Err(AdapterError::EngineFault(format!(
                    "create_view returned {other:?}"
                )))
            }
        };

        let id = TabId::new(self.next_tab);
        self.next_tab += 1;
        let mut tab = Tab::new(id, view);
        tab.url = url;
        self.tabs.insert(id, tab);
        self.by_view.insert(view, id);
        if let Some(w) = self.windows.get_mut(&window) {
            w.add(id);
        }
        Ok(id)
    }

    /// Navigate a tab, applying the policy snapshot first.
    pub fn navigate(&mut self, tab: TabId, url: Url) -> Result<NavigationOutcome, AdapterError> {
        if self.shutting_down {
            return Err(AdapterError::ShuttingDown);
        }
        let view = self
            .tabs
            .get(&tab)
            .map(|t| t.view)
            .ok_or(AdapterError::ViewClosed)?;

        let mut target = url;
        let mut outcome = NavigationOutcome::Started;
        match self.policy.decide_request(&target) {
            RequestDecision::Block => return Ok(NavigationOutcome::Blocked),
            RequestDecision::UpgradeToHttps => {
                let upgraded = upgrade_to_https(&target)?;
                outcome = NavigationOutcome::UpgradedToHttps(upgraded.clone());
                target = upgraded;
            }
            RequestDecision::Allow => {}
        }

        self.engine.execute(Command::Navigate {
            view,
            url: target,
        })?;
        Ok(outcome)
    }

    pub fn close_tab(&mut self, tab: TabId) -> Result<(), AdapterError> {
        let view = self
            .tabs
            .get(&tab)
            .map(|t| t.view)
            .ok_or(AdapterError::ViewClosed)?;
        self.engine.execute(Command::CloseView { view, force: false })?;
        Ok(())
    }

    pub fn close_window(&mut self, window: WindowId) -> Result<(), AdapterError> {
        let tabs: Vec<TabId> = self
            .windows
            .get(&window)
            .map(|w| w.tabs.clone())
            .ok_or(AdapterError::InvalidArgument("unknown window"))?;
        for tab in tabs {
            let _ = self.close_tab(tab);
        }
        Ok(())
    }

    /// Drain engine messages, update the model, answer or queue decisions.
    pub fn pump(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        for message in self.stream.drain() {
            match message {
                EngineMessage::Event(envelope) => self.apply_event(envelope.event, &mut effects),
                EngineMessage::Decision(request) => self.handle_decision(request, &mut effects),
            }
        }
        effects
    }

    fn apply_event(&mut self, event: Event, effects: &mut Vec<Effect>) {
        let tab_id = event.view().and_then(|v| self.by_view.get(&v).copied());

        match event {
            Event::ViewCreated { .. } | Event::ViewReady { .. } | Event::EngineInitialized { .. } => {}
            Event::CloseRequested { .. } => {
                if let Some(tab) = tab_id {
                    effects.push(Effect::CloseRequested(tab));
                }
            }
            Event::ViewClosing { .. } => {}
            Event::ViewClosed { view } => {
                if let Some(tab) = self.by_view.remove(&view) {
                    self.tabs.remove(&tab);
                    for window in self.windows.values_mut() {
                        window.remove(tab);
                    }
                    self.windows.retain(|_, w| !w.is_empty());
                    if let Some(active) = self.active_window {
                        if !self.windows.contains_key(&active) {
                            self.active_window = self.windows.keys().next().copied();
                        }
                    }
                    effects.push(Effect::TabClosed(tab));
                }
            }
            Event::NavigationStarted { url, .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.loading = true;
                    tab.crashed = false;
                    tab.url = Some(url);
                    effects.push(Effect::LoadStarted { tab: tab.id });
                }
            }
            Event::NavigationCommitted { url, .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.url = Some(url.clone());
                    effects.push(Effect::UrlChanged {
                        tab: tab.id,
                        url: url.as_str().to_string(),
                    });
                    self.history.push(HistoryEntry {
                        url,
                        title: tab.title.clone(),
                    });
                }
            }
            Event::LoadProgress { .. } => {}
            Event::LoadFinished { .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.loading = false;
                    effects.push(Effect::LoadFinished { tab: tab.id });
                }
            }
            Event::LoadFailed { reason, .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.loading = false;
                    effects.push(Effect::LoadFailed { tab: tab.id, reason });
                }
            }
            Event::RedirectObserved { .. } => {}
            Event::TitleChanged { title, .. } => {
                let title = sanitize_title(&title);
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.title = title.clone();
                    effects.push(Effect::TitleChanged { tab: tab.id, title });
                }
            }
            Event::UrlChanged { url, .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    if tab.url.as_ref() != Some(&url) {
                        tab.url = Some(url.clone());
                        effects.push(Effect::UrlChanged {
                            tab: tab.id,
                            url: url.as_str().to_string(),
                        });
                    }
                }
            }
            Event::FaviconChanged { .. } | Event::SecurityStateChanged { .. } => {}
            Event::AudioStateChanged { audible, .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.audible = audible;
                }
            }
            Event::RendererTerminated { .. } => {
                if let Some(tab) = tab_id.and_then(|id| self.tabs.get_mut(&id)) {
                    tab.crashed = true;
                    tab.loading = false;
                    effects.push(Effect::RendererCrashed { tab: tab.id });
                }
            }
            Event::ViewUnresponsive { .. } => {
                if let Some(tab) = tab_id {
                    effects.push(Effect::ViewUnresponsive { tab });
                }
            }
            Event::GpuProcessFailed => effects.push(Effect::EngineFault {
                message: "GPU process failed".into(),
            }),
            Event::EngineFatal { message } | Event::AdapterFault { message } => {
                effects.push(Effect::EngineFault { message })
            }
            Event::ShutdownComplete => {
                self.shutdown_complete = true;
                effects.push(Effect::ShutdownComplete);
            }
            Event::EventsDropped { count, .. } => {
                self.dropped_events += count;
            }
        }
    }

    fn handle_decision(&mut self, request: DecisionRequest, effects: &mut Vec<Effect>) {
        let tab = self.by_view.get(&request.view).copied();

        // A decision for a view we no longer track is dropped; the responder's
        // Drop sends the safe default.
        let Some(tab) = tab else {
            return;
        };

        if let DecisionKind::Permission { kind, origin } = &request.kind {
            if let Some(decision) = self.policy.permission_decision(origin, *kind) {
                request.answer(decision);
                return;
            }
            effects.push(Effect::PermissionPrompt {
                tab,
                kind: *kind,
                origin: origin.clone(),
            });
            self.pending.push(request);
            return;
        }

        effects.push(Effect::DecisionRequired {
            tab,
            kind: request.kind.name(),
        });
        self.pending.push(request);
    }

    /// Answer a queued decision. Returns false if the ID is unknown (already
    /// answered or expired).
    pub fn answer_decision(&mut self, id: DecisionId, decision: Decision) -> bool {
        let Some(pos) = self.pending.iter().position(|r| r.id == id) else {
            return false;
        };
        let request = self.pending.remove(pos);
        request.answer(decision);
        true
    }

    /// Drop expired decisions; their responders send the safe default.
    pub fn expire_decisions(&mut self, now: Instant) -> usize {
        let before = self.pending.len();
        self.pending.retain(|r| !r.is_expired(now));
        before - self.pending.len()
    }

    /// Graceful shutdown with escalation: close every view, wait up to
    /// `timeout`, force-close stragglers, then shut the engine down.
    pub fn shutdown(&mut self, timeout: Duration) -> Result<ShutdownReport, AdapterError> {
        self.shutting_down = true;
        let tabs: Vec<TabId> = self.tabs.keys().copied().collect();
        for tab in tabs {
            let _ = self.close_tab(tab);
        }

        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.pump();
            if self.tabs.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        let stragglers: Vec<TabId> = self.tabs.keys().copied().collect();
        for tab in &stragglers {
            if let Some(t) = self.tabs.get(tab) {
                let _ = self.engine.execute(Command::CloseView {
                    view: t.view,
                    force: true,
                });
            }
        }
        let _ = self.pump();

        let report = ShutdownReport {
            forced_closed: stragglers.len(),
            remaining: self.tabs.len(),
            shutdown_complete: false,
        };

        if report.remaining > 0 {
            return Err(AdapterError::EngineFault(format!(
                "{} view(s) still open after force close",
                report.remaining
            )));
        }

        self.engine.shutdown()?;
        let _ = self.pump();

        if !self.shutdown_complete {
            return Err(AdapterError::EngineFault(
                "engine did not confirm shutdown".into(),
            ));
        }

        Ok(ShutdownReport {
            shutdown_complete: true,
            ..report
        })
    }

    /// Snapshot the session for restore after a restart or crash.
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut windows = Vec::new();
        for window in self.windows.values() {
            let tabs: Vec<TabSnapshot> = window
                .tabs
                .iter()
                .filter_map(|id| self.tabs.get(id))
                .map(|tab| TabSnapshot {
                    url: tab.display_url().to_string(),
                    title: tab.title.clone(),
                })
                .collect();
            let active = window
                .active
                .and_then(|a| window.tabs.iter().position(|t| *t == a));
            windows.push(WindowSnapshot { active, tabs });
        }
        SessionSnapshot { windows }
    }

    /// Recreate windows and tabs from a snapshot. URLs that no longer validate
    /// are skipped rather than guessed at.
    pub fn restore(&mut self, snapshot: &SessionSnapshot) -> Result<usize, AdapterError> {
        let mut restored = 0;
        for window_snapshot in &snapshot.windows {
            let window = self.open_window();
            let mut ids = Vec::new();
            for tab_snapshot in &window_snapshot.tabs {
                let url = Url::parse(&tab_snapshot.url).ok();
                let id = self.open_tab(window, url)?;
                if let Some(tab) = self.tabs.get_mut(&id) {
                    tab.title = sanitize_title(&tab_snapshot.title);
                }
                ids.push(id);
                restored += 1;
            }
            if let (Some(index), Some(window)) = (window_snapshot.active, self.windows.get_mut(&window))
            {
                if let Some(id) = ids.get(index) {
                    window.activate(*id);
                }
            }
        }
        Ok(restored)
    }
}

fn upgrade_to_https(url: &Url) -> Result<Url, AdapterError> {
    let rest = url
        .as_str()
        .strip_prefix("http://")
        .ok_or(AdapterError::InvalidArgument("not an http URL"))?;
    Url::parse(&format!("https://{rest}")).map_err(|_| AdapterError::InvalidArgument("bad URL"))
}
