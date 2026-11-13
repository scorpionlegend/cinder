//! Durable shell state: bookmarks, history and settings.
//!
//! The files are line oriented and tab separated, with `%` escaping for the
//! characters a field cannot contain. That keeps the store readable, diffable
//! and dependency free; the format is private to this module.
//!
//! Every write goes to a temporary file in the same directory and is renamed
//! over the old one, so a crash leaves either the previous file or the new one,
//! never a half-written list.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many visits the history keeps before the oldest are dropped.
pub const HISTORY_LIMIT: usize = 5000;

const BOOKMARKS: &str = "bookmarks.tsv";
const HISTORY: &str = "history.tsv";
const SETTINGS: &str = "settings.tsv";

/// A saved page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    /// Unix seconds when the bookmark was added.
    pub added: u64,
}

/// One page visit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: String,
    /// Unix seconds when the page was committed.
    pub visited: u64,
}

/// Bookmarks, history and settings rooted at one profile directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    bookmarks: Vec<Bookmark>,
    history: Vec<Visit>,
    settings: BTreeMap<String, String>,
}

impl Store {
    /// Open (or create) the store under `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let bookmarks = read_lines(&dir.join(BOOKMARKS))
            .into_iter()
            .filter_map(|fields| {
                Some(Bookmark {
                    added: fields.first()?.parse().ok()?,
                    url: fields.get(1)?.clone(),
                    title: fields.get(2).cloned().unwrap_or_default(),
                })
            })
            .collect();
        let history = read_lines(&dir.join(HISTORY))
            .into_iter()
            .filter_map(|fields| {
                Some(Visit {
                    visited: fields.first()?.parse().ok()?,
                    url: fields.get(1)?.clone(),
                    title: fields.get(2).cloned().unwrap_or_default(),
                })
            })
            .collect();
        let settings = read_lines(&dir.join(SETTINGS))
            .into_iter()
            .filter_map(|fields| Some((fields.first()?.clone(), fields.get(1).cloned().unwrap_or_default())))
            .collect();
        Ok(Self {
            dir,
            bookmarks,
            history,
            settings,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bookmarks in the order they were added.
    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }

    /// Add a bookmark, or update the title of one that already exists.
    /// Returns true when a new bookmark was added.
    pub fn add_bookmark(&mut self, url: &str, title: &str) -> io::Result<bool> {
        let url = url.trim();
        if url.is_empty() {
            return Ok(false);
        }
        let title = title.trim();
        if let Some(existing) = self.bookmarks.iter_mut().find(|b| b.url == url) {
            if !title.is_empty() && existing.title != title {
                existing.title = title.to_string();
                self.save_bookmarks()?;
            }
            return Ok(false);
        }
        self.bookmarks.push(Bookmark {
            url: url.to_string(),
            title: title.to_string(),
            added: now(),
        });
        self.save_bookmarks()?;
        Ok(true)
    }

    /// Remove a bookmark by URL. Returns true when one was removed.
    pub fn remove_bookmark(&mut self, url: &str) -> io::Result<bool> {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.url != url.trim());
        let removed = self.bookmarks.len() != before;
        if removed {
            self.save_bookmarks()?;
        }
        Ok(removed)
    }

    /// Whether `url` is bookmarked.
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url.trim())
    }

    /// Visits in the order they happened.
    pub fn history(&self) -> &[Visit] {
        &self.history
    }

    /// The newest `limit` visits, newest first.
    pub fn recent(&self, limit: usize) -> Vec<&Visit> {
        self.history.iter().rev().take(limit).collect()
    }

    /// Record a committed navigation. Empty URLs are ignored.
    pub fn record_visit(&mut self, url: &str, title: &str) -> io::Result<()> {
        if url.trim().is_empty() {
            return Ok(());
        }
        self.push_visit(url, title);
        self.save_history()
    }

    /// Append a visit and enforce the cap, without touching the disk.
    fn push_visit(&mut self, url: &str, title: &str) {
        self.history.push(Visit {
            url: url.trim().to_string(),
            title: title.trim().to_string(),
            visited: now(),
        });
        if self.history.len() > HISTORY_LIMIT {
            let excess = self.history.len() - HISTORY_LIMIT;
            self.history.drain(0..excess);
        }
    }

    /// Drop every visit.
    pub fn clear_history(&mut self) -> io::Result<()> {
        self.history.clear();
        self.save_history()
    }

    /// A setting, or `None` when it was never written.
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    /// Write a setting and persist the file.
    pub fn set_setting(&mut self, key: &str, value: &str) -> io::Result<()> {
        self.settings.insert(key.to_string(), value.to_string());
        self.save_settings()
    }

    /// All settings, sorted by key.
    pub fn settings(&self) -> &BTreeMap<String, String> {
        &self.settings
    }

    /// The page a new window opens when no URL is given.
    pub fn homepage(&self) -> &str {
        self.setting("homepage").unwrap_or("about:blank")
    }

    /// Whether the last session should be restored on start.
    pub fn restore_session(&self) -> bool {
        self.setting("restore_session")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true)
    }

    fn save_bookmarks(&self) -> io::Result<()> {
        let rows: Vec<Vec<String>> = self
            .bookmarks
            .iter()
            .map(|b| vec![b.added.to_string(), b.url.clone(), b.title.clone()])
            .collect();
        write_lines(&self.dir.join(BOOKMARKS), &rows)
    }

    fn save_history(&self) -> io::Result<()> {
        let rows: Vec<Vec<String>> = self
            .history
            .iter()
            .map(|v| vec![v.visited.to_string(), v.url.clone(), v.title.clone()])
            .collect();
        write_lines(&self.dir.join(HISTORY), &rows)
    }

    fn save_settings(&self) -> io::Result<()> {
        let rows: Vec<Vec<String>> = self
            .settings
            .iter()
            .map(|(k, v)| vec![k.clone(), v.clone()])
            .collect();
        write_lines(&self.dir.join(SETTINGS), &rows)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_lines(path: &Path) -> Vec<Vec<String>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split('\t').map(unescape).collect())
        .collect()
}

fn write_lines(path: &Path, rows: &[Vec<String>]) -> io::Result<()> {
    let mut text = String::new();
    for row in rows {
        let fields: Vec<String> = row.iter().map(|field| escape(field)).collect();
        text.push_str(&fields.join("\t"));
        text.push('\n');
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let hex: String = chars.by_ref().take(2).collect();
        match hex.as_str() {
            "25" => out.push('%'),
            "09" => out.push('\t'),
            "0A" => out.push('\n'),
            "0D" => out.push('\r'),
            _ => {
                out.push('%');
                out.push_str(&hex);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cinder-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn bookmarks_survive_a_reopen_and_do_not_duplicate() {
        let dir = scratch("bookmarks");
        {
            let mut store = Store::open(&dir).unwrap();
            assert!(store.add_bookmark("https://example.com", "Example").unwrap());
            assert!(!store.add_bookmark("https://example.com", "Example 2").unwrap());
            assert!(store.is_bookmarked("https://example.com"));
            assert_eq!(store.bookmarks().len(), 1);
            assert_eq!(store.bookmarks()[0].title, "Example 2");
        }
        let mut store = Store::open(&dir).unwrap();
        assert_eq!(store.bookmarks().len(), 1);
        assert!(store.remove_bookmark("https://example.com").unwrap());
        assert!(!store.remove_bookmark("https://example.com").unwrap());
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert!(store.bookmarks().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_keeps_order_and_recent_is_newest_first() {
        let dir = scratch("history");
        let mut store = Store::open(&dir).unwrap();
        store.record_visit("https://a.example", "A").unwrap();
        store.record_visit("https://b.example", "B").unwrap();
        assert_eq!(store.history().len(), 2);
        assert_eq!(store.recent(1)[0].url, "https://b.example");
        store.clear_history().unwrap();
        assert!(store.history().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_capped() {
        let dir = scratch("cap");
        let mut store = Store::open(&dir).unwrap();
        for i in 0..HISTORY_LIMIT + 5 {
            store.push_visit(&format!("https://example.com/{i}"), "");
        }
        assert_eq!(store.history().len(), HISTORY_LIMIT);
        assert_eq!(store.history()[0].url, "https://example.com/5");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_with_defaults() {
        let dir = scratch("settings");
        {
            let mut store = Store::open(&dir).unwrap();
            assert_eq!(store.homepage(), "about:blank");
            assert!(store.restore_session());
            store.set_setting("homepage", "https://example.com").unwrap();
            store.set_setting("restore_session", "false").unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.homepage(), "https://example.com");
        assert!(!store.restore_session());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fields_with_tabs_and_newlines_round_trip() {
        let dir = scratch("escape");
        let mut store = Store::open(&dir).unwrap();
        store.add_bookmark("https://example.com/a%20b", "tab\there\nline").unwrap();
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.bookmarks()[0].url, "https://example.com/a%20b");
        assert_eq!(store.bookmarks()[0].title, "tab\there\nline");
        let _ = fs::remove_dir_all(&dir);
    }
}
