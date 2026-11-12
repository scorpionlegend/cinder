//! Cinder-side tab and window model. This is the authoritative copy for the
//! shell; the engine's view is an implementation detail behind `ViewId`.

use std::fmt;

use cinder_adapter::Url;
use cinder_adapter::ViewId;

macro_rules! local_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub fn get(self) -> u64 {
                self.0
            }

            pub(crate) fn new(raw: u64) -> Self {
                Self(raw)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
    };
}

local_id!(TabId, "tab-");
local_id!(WindowId, "window-");

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub id: TabId,
    pub view: ViewId,
    pub url: Option<Url>,
    pub title: String,
    pub loading: bool,
    pub crashed: bool,
    pub audible: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

impl Tab {
    pub fn new(id: TabId, view: ViewId) -> Self {
        Self {
            id,
            view,
            url: None,
            title: String::new(),
            loading: false,
            crashed: false,
            audible: false,
            can_go_back: false,
            can_go_forward: false,
        }
    }

    /// What the tab strip shows.
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        match &self.url {
            Some(url) => url.as_str().to_string(),
            None => "New Tab".to_string(),
        }
    }

    pub fn display_url(&self) -> &str {
        self.url.as_ref().map(Url::as_str).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub id: WindowId,
    pub tabs: Vec<TabId>,
    pub active: Option<TabId>,
}

impl Window {
    pub fn new(id: WindowId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: None,
        }
    }

    pub fn add(&mut self, tab: TabId) {
        if !self.tabs.contains(&tab) {
            self.tabs.push(tab);
        }
        if self.active.is_none() {
            self.active = Some(tab);
        }
    }

    pub fn remove(&mut self, tab: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| *t == tab) else {
            return false;
        };
        self.tabs.remove(pos);
        if self.active == Some(tab) {
            self.active = self
                .tabs
                .get(pos.saturating_sub(1))
                .or_else(|| self.tabs.first())
                .copied();
        }
        true
    }

    pub fn activate(&mut self, tab: TabId) -> bool {
        if self.tabs.contains(&tab) {
            self.active = Some(tab);
            true
        } else {
            false
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinder_adapter::IdAllocator;

    fn tab(n: u64) -> TabId {
        TabId::new(n)
    }

    #[test]
    fn removing_the_active_tab_selects_a_neighbour() {
        let mut w = Window::new(WindowId::new(1));
        w.add(tab(1));
        w.add(tab(2));
        w.add(tab(3));
        w.activate(tab(2));
        assert!(w.remove(tab(2)));
        assert_eq!(w.active, Some(tab(1)));
        assert_eq!(w.tabs, vec![tab(1), tab(3)]);
    }

    #[test]
    fn removing_the_last_tab_clears_active() {
        let mut w = Window::new(WindowId::new(1));
        w.add(tab(1));
        assert!(w.remove(tab(1)));
        assert_eq!(w.active, None);
        assert!(w.is_empty());
    }

    #[test]
    fn display_title_falls_back_to_url_then_placeholder() {
        let alloc = IdAllocator::new();
        let mut t = Tab::new(tab(1), alloc.next_view());
        assert_eq!(t.display_title(), "New Tab");
        t.url = Some(Url::parse("https://example.com/x").unwrap());
        assert_eq!(t.display_title(), "https://example.com/x");
        t.title = "Example".into();
        assert_eq!(t.display_title(), "Example");
    }
}
