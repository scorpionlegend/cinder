//! Capability negotiation (spec §9). Product code checks capabilities, never
//! engine versions.

use crate::command::{Command, Surface};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub incognito_contexts: bool,
    pub offscreen_rendering: bool,
    pub download_pause: bool,
    pub print_to_pdf: bool,
    pub find_in_page: bool,
    pub devtools: bool,
    /// Request interception / HTTPS upgrade hooks are usable in handlers.
    pub request_interception: bool,
}

impl Capabilities {
    /// Nothing beyond navigation and lifecycle.
    pub const fn minimal() -> Self {
        Self {
            incognito_contexts: false,
            offscreen_rendering: false,
            download_pause: false,
            print_to_pdf: false,
            find_in_page: false,
            devtools: false,
            request_interception: false,
        }
    }

    /// What the CEF baseline is *expected* to provide. This is an expectation
    /// recorded for the Phase 0 coverage table, not a verified result: the
    /// CEF-backed adapter must report its real set at initialization.
    pub const fn cef_baseline_expected() -> Self {
        Self {
            incognito_contexts: true,
            offscreen_rendering: true,
            download_pause: false,
            print_to_pdf: true,
            find_in_page: true,
            devtools: true,
            request_interception: true,
        }
    }

    pub fn supports(&self, command: &Command) -> bool {
        match command {
            Command::CreateProfile { incognito, .. } => !*incognito || self.incognito_contexts,
            Command::CreateView { surface, .. } => match surface {
                Surface::NativeChild { .. } => true,
                Surface::Offscreen { .. } => self.offscreen_rendering,
            },
            Command::FindInPage { .. } => self.find_in_page,
            Command::InstallPolicy { .. } => self.request_interception,
            Command::Reload { .. }
            | Command::Stop { .. }
            | Command::GoBack { .. }
            | Command::GoForward { .. }
            | Command::Navigate { .. }
            | Command::SetZoom { .. }
            | Command::MuteAudio { .. }
            | Command::Resize { .. }
            | Command::SetVisible { .. }
            | Command::SetFocus { .. }
            | Command::CloseView { .. }
            | Command::CloseProfile { .. }
            | Command::ClearData { .. }
            | Command::Shutdown => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ViewId;
    use crate::sanitize::Url;

    #[test]
    fn minimal_rejects_offscreen_and_incognito() {
        let caps = Capabilities::minimal();
        assert!(!caps.supports(&Command::CreateView {
            profile: crate::ids::ProfileId::from_raw(1),
            surface: Surface::Offscreen {
                width: 800,
                height: 600
            },
            initial_url: None,
        }));
        assert!(!caps.supports(&Command::CreateProfile {
            path: "/tmp/p".into(),
            incognito: true
        }));
        assert!(caps.supports(&Command::Navigate {
            view: ViewId::from_raw(1),
            url: Url::parse("https://example.com").unwrap()
        }));
    }
}
