//! Commands: Cinder → engine (spec §4). Every command returns a typed result;
//! effects are reported through events, never by blocking.

use std::sync::Arc;

use crate::ids::{ProfileId, ViewId};
use crate::policy::PolicySnapshot;
use crate::sanitize::Url;

/// Where a view's pixels appear. Part of the API so U1 (native child view) and
/// U2 (off-screen) are interchangeable behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Surface {
    /// A child window the engine creates inside the shell's window. `parent` is
    /// the shell's platform handle — an X11 window id on Linux, an `HWND` on
    /// Windows, an `NSView` pointer on macOS — and zero means "no parent", which
    /// is invalid.
    NativeChild {
        parent: u64,
        /// Offset of the page area from the top of the parent, in physical
        /// pixels: the toolbar occupies that strip.
        top: u32,
        width: u32,
        height: u32,
        scale: f32,
    },
    Offscreen { width: u32, height: u32 },
}

impl Surface {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::NativeChild { width, height, .. } => (*width, *height),
            Self::Offscreen { width, height } => (*width, *height),
        }
    }
}

/// Categories for `ClearData`. Chromium owns the storage; Cinder only asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataCategories {
    pub cookies: bool,
    pub cache: bool,
    pub site_storage: bool,
    pub history: bool,
}

impl DataCategories {
    pub const fn all() -> Self {
        Self {
            cookies: true,
            cache: true,
            site_storage: true,
            history: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        !(self.cookies || self.cache || self.site_storage || self.history)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // Engine lifecycle
    Shutdown,

    // Profiles
    CreateProfile {
        path: String,
        incognito: bool,
    },
    CloseProfile {
        profile: ProfileId,
    },
    ClearData {
        profile: ProfileId,
        categories: DataCategories,
        /// Unix seconds; `None` clears everything.
        since: Option<u64>,
    },

    // View lifecycle
    CreateView {
        profile: ProfileId,
        surface: Surface,
        initial_url: Option<Url>,
    },
    CloseView {
        view: ViewId,
        force: bool,
    },
    SetVisible {
        view: ViewId,
        visible: bool,
    },
    SetFocus {
        view: ViewId,
        focused: bool,
    },
    Resize {
        view: ViewId,
        width: u32,
        height: u32,
        scale: f32,
    },

    // Navigation
    Navigate {
        view: ViewId,
        url: Url,
    },
    Reload {
        view: ViewId,
        bypass_cache: bool,
    },
    Stop {
        view: ViewId,
    },
    GoBack {
        view: ViewId,
    },
    GoForward {
        view: ViewId,
    },

    // Page features
    FindInPage {
        view: ViewId,
        query: String,
        forward: bool,
    },
    SetZoom {
        view: ViewId,
        factor: f32,
    },
    MuteAudio {
        view: ViewId,
        muted: bool,
    },

    // Policy
    InstallPolicy {
        snapshot: Arc<PolicySnapshot>,
    },
}

impl Command {
    /// The view this command targets, if any.
    pub fn view(&self) -> Option<ViewId> {
        match self {
            Self::CloseView { view, .. }
            | Self::SetVisible { view, .. }
            | Self::SetFocus { view, .. }
            | Self::Resize { view, .. }
            | Self::Navigate { view, .. }
            | Self::Reload { view, .. }
            | Self::Stop { view }
            | Self::GoBack { view }
            | Self::GoForward { view }
            | Self::FindInPage { view, .. }
            | Self::SetZoom { view, .. }
            | Self::MuteAudio { view, .. } => Some(*view),
            Self::Shutdown
            | Self::CreateProfile { .. }
            | Self::CloseProfile { .. }
            | Self::ClearData { .. }
            | Self::CreateView { .. }
            | Self::InstallPolicy { .. } => None,
        }
    }

    /// Stable name for logs and diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::CreateProfile { .. } => "create_profile",
            Self::CloseProfile { .. } => "close_profile",
            Self::ClearData { .. } => "clear_data",
            Self::CreateView { .. } => "create_view",
            Self::CloseView { .. } => "close_view",
            Self::SetVisible { .. } => "set_visible",
            Self::SetFocus { .. } => "set_focus",
            Self::Resize { .. } => "resize",
            Self::Navigate { .. } => "navigate",
            Self::Reload { .. } => "reload",
            Self::Stop { .. } => "stop",
            Self::GoBack { .. } => "go_back",
            Self::GoForward { .. } => "go_forward",
            Self::FindInPage { .. } => "find_in_page",
            Self::SetZoom { .. } => "set_zoom",
            Self::MuteAudio { .. } => "mute_audio",
            Self::InstallPolicy { .. } => "install_policy",
        }
    }

    /// Validate arguments that the type system cannot (spec §10: engine input
    /// is untrusted, but so is caller input).
    pub fn validate(&self) -> Result<(), crate::AdapterError> {
        use crate::AdapterError;
        match self {
            Self::CreateView { surface, .. } => {
                let (w, h) = surface.size();
                if w == 0 || h == 0 {
                    return Err(AdapterError::InvalidArgument("surface has a zero dimension"));
                }
                if let Surface::NativeChild { parent, scale, .. } = surface {
                    if *parent == 0 {
                        return Err(AdapterError::InvalidArgument("native surface has no parent"));
                    }
                    if !scale.is_finite() || *scale <= 0.0 {
                        return Err(AdapterError::InvalidArgument("scale must be positive"));
                    }
                }
                Ok(())
            }
            Self::Resize { width, height, scale, .. } => {
                if *width == 0 || *height == 0 {
                    return Err(AdapterError::InvalidArgument("zero view size"));
                }
                if !scale.is_finite() || *scale <= 0.0 {
                    return Err(AdapterError::InvalidArgument("scale must be positive"));
                }
                Ok(())
            }
            Self::SetZoom { factor, .. } => {
                if !factor.is_finite() || *factor <= 0.0 || *factor > 10.0 {
                    return Err(AdapterError::InvalidArgument("zoom factor out of range"));
                }
                Ok(())
            }
            Self::FindInPage { query, .. } => {
                if query.is_empty() {
                    return Err(AdapterError::InvalidArgument("empty find query"));
                }
                if query.chars().count() > 512 {
                    return Err(AdapterError::InvalidArgument("find query too long"));
                }
                Ok(())
            }
            Self::ClearData { categories, .. } => {
                if categories.is_empty() {
                    return Err(AdapterError::InvalidArgument("no data categories selected"));
                }
                Ok(())
            }
            Self::CreateProfile { path, .. } => {
                if path.is_empty() {
                    return Err(AdapterError::InvalidArgument("empty profile path"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Result of an accepted command. Completion of effects arrives as events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    Done,
    ViewCreated(ViewId),
    ProfileCreated(ProfileId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_sized_surfaces() {
        let cmd = Command::CreateView {
            profile: ProfileId::from_raw(1),
            surface: Surface::Offscreen { width: 0, height: 10 },
            initial_url: None,
        };
        assert!(cmd.validate().is_err());
    }

    #[test]
    fn rejects_native_surface_without_parent() {
        let cmd = Command::CreateView {
            profile: ProfileId::from_raw(1),
            surface: Surface::NativeChild {
                parent: 0,
                top: 0,
                width: 10,
                height: 10,
                scale: 1.0,
            },
            initial_url: None,
        };
        assert!(cmd.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_zoom() {
        let cmd = Command::SetZoom {
            view: ViewId::from_raw(1),
            factor: 0.0,
        };
        assert!(cmd.validate().is_err());
    }

    #[test]
    fn empty_clear_data_is_invalid() {
        let cmd = Command::ClearData {
            profile: ProfileId::from_raw(1),
            categories: DataCategories::default(),
            since: None,
        };
        assert!(cmd.validate().is_err());
    }
}
