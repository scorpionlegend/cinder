//! End-to-end orchestrator tests against the mock engine.

use std::sync::Arc;
use std::time::Duration;

use cinder_adapter::decision::{Decision, PermissionKind};
use cinder_adapter::policy::PolicySnapshot;
use cinder_adapter::sanitize::Url;
use cinder_adapter_mock::MockEngine;
use cinder_core::{Browser, Effect, NavigationOutcome};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn browser(engine: MockEngine, policy: PolicySnapshot) -> Browser {
    Browser::new(Arc::new(engine), Arc::new(policy), "/tmp/cinder-test-profile").unwrap()
}

#[test]
fn navigation_updates_title_url_and_history() {
    let engine = MockEngine::new();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    let tab = b.open_tab(w, None).unwrap();
    assert_eq!(
        b.navigate(tab, url("https://example.com/")).unwrap(),
        NavigationOutcome::Started
    );

    let effects = b.pump();
    assert!(effects.iter().any(|e| matches!(e, Effect::LoadFinished { .. })));
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::TitleChanged { .. })));

    let t = b.tab(tab).unwrap();
    assert_eq!(t.display_url(), "https://example.com/");
    assert!(!t.loading);
    assert_eq!(b.history().len(), 1);
    assert_eq!(b.history()[0].url.as_str(), "https://example.com/");
}

#[test]
fn policy_blocks_and_upgrades_before_the_engine_sees_the_url() {
    let engine = MockEngine::new();
    let policy = PolicySnapshot::new(1)
        .with_blocked_host("doubleclick.net")
        .with_https_upgrade(true);
    let mut b = browser(engine, policy);
    let w = b.open_window();
    let tab = b.open_tab(w, None).unwrap();

    assert_eq!(
        b.navigate(tab, url("https://ads.doubleclick.net/x")).unwrap(),
        NavigationOutcome::Blocked
    );
    assert_eq!(
        b.navigate(tab, url("http://example.com/")).unwrap(),
        NavigationOutcome::UpgradedToHttps(url("https://example.com/"))
    );

    b.pump();
    assert_eq!(b.tab(tab).unwrap().display_url(), "https://example.com/");
}

#[test]
fn permission_prompt_is_queued_then_answered() {
    let engine = MockEngine::builder()
        .request_permission(PermissionKind::Geolocation)
        .build();
    let mut b = browser(engine.clone(), PolicySnapshot::new(1));
    let w = b.open_window();
    let tab = b.open_tab(w, Some(url("https://maps.example/"))).unwrap();
    let _ = tab;

    let effects = b.pump();
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::PermissionPrompt {
            kind: PermissionKind::Geolocation,
            ..
        }
    )));
    assert_eq!(b.pending_decisions().len(), 1);

    let id = b.pending_decisions()[0].id;
    assert!(b.answer_decision(id, Decision::AllowOnce));
    assert_eq!(engine.decisions(), vec![(id, Decision::AllowOnce)]);
    assert!(b.pending_decisions().is_empty());
}

#[test]
fn stored_permission_decision_skips_the_prompt() {
    let engine = MockEngine::builder()
        .request_permission(PermissionKind::Geolocation)
        .build();
    let policy = PolicySnapshot::new(1).with_permission(
        "https://maps.example",
        PermissionKind::Geolocation,
        Decision::Deny,
    );
    let mut b = browser(engine.clone(), policy);
    let w = b.open_window();
    b.open_tab(w, Some(url("https://maps.example/"))).unwrap();

    let effects = b.pump();
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::PermissionPrompt { .. })));
    assert!(b.pending_decisions().is_empty());
    assert_eq!(engine.decisions().len(), 1);
    assert_eq!(engine.decisions()[0].1, Decision::Deny);
}

#[test]
fn expired_decision_falls_back_to_deny() {
    let engine = MockEngine::builder()
        .request_permission(PermissionKind::Camera)
        .build();
    let mut b = browser(engine.clone(), PolicySnapshot::new(1));
    let w = b.open_window();
    b.open_tab(w, Some(url("https://meet.example/"))).unwrap();
    b.pump();
    assert_eq!(b.pending_decisions().len(), 1);

    let expired = b.expire_decisions(std::time::Instant::now() + Duration::from_secs(3600));
    assert_eq!(expired, 1);
    assert_eq!(engine.decisions()[0].1, Decision::Deny);
}

#[test]
fn renderer_crash_is_reported_and_tab_stays() {
    let engine = MockEngine::builder().crash_after_load(true).build();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    let tab = b.open_tab(w, Some(url("https://crash.example/"))).unwrap();

    let effects = b.pump();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::RendererCrashed { tab: t } if *t == tab)));
    assert!(b.tab(tab).unwrap().crashed);
    assert_eq!(b.tab_count(), 1);
}

#[test]
fn closing_a_tab_removes_it_from_the_window() {
    let engine = MockEngine::new();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    let tab = b.open_tab(w, None).unwrap();
    b.close_tab(tab).unwrap();
    let effects = b.pump();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::TabClosed { .. })));
    assert_eq!(b.tab_count(), 0);
    assert!(b.window(w).is_none(), "empty windows are dropped");
}

#[test]
fn session_snapshot_restores_windows_tabs_and_active_tab() {
    let engine = MockEngine::new();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    let first = b.open_tab(w, Some(url("https://example.com/"))).unwrap();
    let second = b.open_tab(w, Some(url("https://news.example/"))).unwrap();
    b.pump();
    b.window_mut(w).unwrap().activate(second);
    let _ = first;

    let snapshot = b.snapshot();
    let encoded = snapshot.encode();
    let decoded = cinder_core::SessionSnapshot::decode(&encoded).unwrap();
    assert_eq!(snapshot, decoded);

    let mut restored = browser(MockEngine::new(), PolicySnapshot::new(2));
    let count = restored.restore(&decoded).unwrap();
    assert_eq!(count, 2);
    assert_eq!(restored.tab_count(), 2);
    let window = restored.windows().next().unwrap();
    assert_eq!(window.tabs.len(), 2);
    assert_eq!(window.active, Some(window.tabs[1]));
}

#[test]
fn shutdown_escalates_when_a_view_refuses_to_close() {
    let engine = MockEngine::builder().refuse_close(true).build();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    b.open_tab(w, Some(url("https://example.com/"))).unwrap();
    b.pump();

    let report = b.shutdown(Duration::from_millis(20)).unwrap();
    assert_eq!(report.forced_closed, 1);
    assert_eq!(report.remaining, 0);
    assert!(report.shutdown_complete);
    assert!(b.shutdown_complete());
}

#[test]
fn commands_after_shutdown_are_rejected() {
    let engine = MockEngine::new();
    let mut b = browser(engine, PolicySnapshot::new(1));
    let w = b.open_window();
    b.shutdown(Duration::from_millis(20)).unwrap();
    assert!(b.open_tab(w, None).is_err());
}
