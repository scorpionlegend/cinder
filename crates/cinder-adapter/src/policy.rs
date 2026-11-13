//! Immutable, versioned policy snapshots (spec §8). The adapter consults the
//! current snapshot inside synchronous engine callbacks; it never calls up into
//! product code from an engine thread.

use crate::decision::{Decision, PermissionKind};
use crate::sanitize::Url;

/// Answer for a request the engine needs synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDecision {
    Allow,
    Block,
    UpgradeToHttps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionDefault {
    origin: String,
    kind: PermissionKind,
    decision: Decision,
}

/// One compiled policy version. Swapped atomically; never mutated in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySnapshot {
    version: u64,
    blocked_hosts: Vec<String>,
    https_upgrade: bool,
    permission_defaults: Vec<PermissionDefault>,
}

impl PolicySnapshot {
    pub fn new(version: u64) -> Self {
        Self {
            version,
            blocked_hosts: Vec::new(),
            https_upgrade: false,
            permission_defaults: Vec::new(),
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Block a host and its subdomains. `"doubleclick.net"` also blocks
    /// `ads.doubleclick.net`, but not `notdoubleclick.net`.
    pub fn with_blocked_host(mut self, host: &str) -> Self {
        let host = host.trim().trim_start_matches('.').to_ascii_lowercase();
        if !host.is_empty() && !self.blocked_hosts.contains(&host) {
            self.blocked_hosts.push(host);
        }
        self
    }

    pub fn with_https_upgrade(mut self, enabled: bool) -> Self {
        self.https_upgrade = enabled;
        self
    }

    /// Store a permission decision. `origin` is `scheme://host` or `"*"`.
    pub fn with_permission(
        mut self,
        origin: &str,
        kind: PermissionKind,
        decision: Decision,
    ) -> Self {
        let origin = origin.trim().to_ascii_lowercase();
        self.permission_defaults
            .retain(|p| !(p.origin == origin && p.kind == kind));
        self.permission_defaults.push(PermissionDefault {
            origin,
            kind,
            decision,
        });
        self
    }

    pub fn blocked_hosts(&self) -> &[String] {
        &self.blocked_hosts
    }

    pub fn https_upgrade(&self) -> bool {
        self.https_upgrade
    }

    pub fn is_host_blocked(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.blocked_hosts
            .iter()
            .any(|blocked| host == *blocked || host.ends_with(&format!(".{blocked}")))
    }

    /// Decide a top-level or subresource request.
    pub fn decide_request(&self, url: &Url) -> RequestDecision {
        if let Some(host) = url.host() {
            if self.is_host_blocked(&host) {
                return RequestDecision::Block;
            }
        }
        // Local development servers rarely speak TLS, and upgrading them turns
        // a working page into a handshake failure. Browsers exempt loopback for
        // the same reason.
        if self.https_upgrade && url.scheme() == "http" && !url.host().is_some_and(|h| is_loopback(&h))
        {
            return RequestDecision::UpgradeToHttps;
        }
        RequestDecision::Allow
    }

    /// Stored decision for a permission, exact origin first, then `"*"`.
    pub fn permission_decision(&self, origin: &str, kind: PermissionKind) -> Option<Decision> {
        let origin = origin.to_ascii_lowercase();
        self.permission_defaults
            .iter()
            .find(|p| p.origin == origin && p.kind == kind)
            .or_else(|| {
                self.permission_defaults
                    .iter()
                    .find(|p| p.origin == "*" && p.kind == kind)
            })
            .map(|p| p.decision)
    }
}

/// Whether a lowercased host names this machine, where an HTTP server is
/// normally a local development server rather than something to upgrade.
fn is_loopback(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") || host == "[::1]" || host == "::1" {
        return true;
    }
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn blocks_host_and_subdomains_only() {
        let p = PolicySnapshot::new(1).with_blocked_host("doubleclick.net");
        assert_eq!(p.decide_request(&url("https://doubleclick.net/x")), RequestDecision::Block);
        assert_eq!(
            p.decide_request(&url("https://ads.doubleclick.net/x")),
            RequestDecision::Block
        );
        assert_eq!(
            p.decide_request(&url("https://notdoubleclick.net/")),
            RequestDecision::Allow
        );
    }

    #[test]
    fn upgrades_plain_http_when_enabled() {
        let p = PolicySnapshot::new(2).with_https_upgrade(true);
        assert_eq!(
            p.decide_request(&url("http://example.com/")),
            RequestDecision::UpgradeToHttps
        );
        assert_eq!(
            p.decide_request(&url("https://example.com/")),
            RequestDecision::Allow
        );
    }

    #[test]
    fn leaves_loopback_on_plain_http() {
        let p = PolicySnapshot::new(2).with_https_upgrade(true);
        for host in [
            "http://localhost:8099/",
            "http://127.0.0.1:8099/",
            "http://127.0.0.53/",
            "http://[::1]:8099/",
            "http://app.localhost/",
        ] {
            assert_eq!(
                p.decide_request(&url(host)),
                RequestDecision::Allow,
                "{host} should not be upgraded"
            );
        }
        assert_eq!(
            p.decide_request(&url("http://192.168.1.10/")),
            RequestDecision::UpgradeToHttps
        );
    }

    #[test]
    fn permission_lookup_prefers_exact_origin_then_wildcard() {
        let p = PolicySnapshot::new(3)
            .with_permission("*", PermissionKind::Notifications, Decision::Deny)
            .with_permission("https://meet.example", PermissionKind::Camera, Decision::Allow);
        assert_eq!(
            p.permission_decision("https://meet.example", PermissionKind::Camera),
            Some(Decision::Allow)
        );
        assert_eq!(
            p.permission_decision("https://other.example", PermissionKind::Notifications),
            Some(Decision::Deny)
        );
        assert_eq!(
            p.permission_decision("https://other.example", PermissionKind::Camera),
            None
        );
    }
}
