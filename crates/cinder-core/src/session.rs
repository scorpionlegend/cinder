//! Session snapshots: the Cinder-owned record used to restore windows and tabs
//! after a restart or a browser-process crash (architecture §10.3).
//!
//! Format is line-oriented and versioned. Fields are percent-encoded so a URL
//! or title containing tabs/newlines cannot corrupt the file.

use std::fmt;

pub const SESSION_FORMAT_VERSION: u32 = 1;
const HEADER: &str = "cinder-session";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionSnapshot {
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowSnapshot {
    /// Index into `tabs` of the tab that was active.
    pub active: Option<usize>,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSnapshot {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    MissingHeader,
    UnsupportedVersion(u32),
    Malformed(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => f.write_str("not a Cinder session file"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported session version {v}"),
            Self::Malformed(why) => write!(f, "malformed session file: {why}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl SessionSnapshot {
    pub fn encode(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{HEADER}\t{SESSION_FORMAT_VERSION}\n"));
        for window in &self.windows {
            let active = match window.active {
                Some(i) => i.to_string(),
                None => "-".to_string(),
            };
            out.push_str(&format!("window\tactive={active}\n"));
            for tab in &window.tabs {
                out.push_str(&format!(
                    "tab\t{}\t{}\n",
                    encode_component(&tab.url),
                    encode_component(&tab.title)
                ));
            }
        }
        out
    }

    pub fn decode(input: &str) -> Result<Self, SessionError> {
        let mut lines = input.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next().ok_or(SessionError::MissingHeader)?;
        let mut parts = header.split('\t');
        if parts.next() != Some(HEADER) {
            return Err(SessionError::MissingHeader);
        }
        let version: u32 = parts
            .next()
            .ok_or(SessionError::MissingHeader)?
            .parse()
            .map_err(|_| SessionError::MissingHeader)?;
        if version != SESSION_FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion(version));
        }

        let mut snapshot = SessionSnapshot::default();
        for line in lines {
            let mut fields = line.split('\t');
            match fields.next() {
                Some("window") => {
                    let active_field = fields
                        .next()
                        .and_then(|f| f.strip_prefix("active="))
                        .ok_or_else(|| SessionError::Malformed("window without active=".into()))?;
                    let active = if active_field == "-" {
                        None
                    } else {
                        Some(
                            active_field
                                .parse()
                                .map_err(|_| SessionError::Malformed("bad active index".into()))?,
                        )
                    };
                    snapshot.windows.push(WindowSnapshot {
                        active,
                        tabs: Vec::new(),
                    });
                }
                Some("tab") => {
                    let url = fields
                        .next()
                        .ok_or_else(|| SessionError::Malformed("tab without url".into()))?;
                    let title = fields
                        .next()
                        .ok_or_else(|| SessionError::Malformed("tab without title".into()))?;
                    let window = snapshot
                        .windows
                        .last_mut()
                        .ok_or_else(|| SessionError::Malformed("tab before window".into()))?;
                    window.tabs.push(TabSnapshot {
                        url: decode_component(url)?,
                        title: decode_component(title)?,
                    });
                }
                Some(other) => {
                    return Err(SessionError::Malformed(format!("unknown record {other:?}")))
                }
                None => {}
            }
        }
        Ok(snapshot)
    }
}

fn encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn decode_component(input: &str) -> Result<String, SessionError> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(SessionError::Malformed("truncated escape".into()));
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .map_err(|_| SessionError::Malformed("bad escape".into()))?;
            let value =
                u8::from_str_radix(hex, 16).map_err(|_| SessionError::Malformed("bad escape".into()))?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| SessionError::Malformed("invalid utf-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionSnapshot {
        SessionSnapshot {
            windows: vec![
                WindowSnapshot {
                    active: Some(1),
                    tabs: vec![
                        TabSnapshot {
                            url: "https://example.com/".into(),
                            title: "Example".into(),
                        },
                        TabSnapshot {
                            url: "https://example.com/a?b=c#d".into(),
                            title: "Tab\twith\ttabs\nand newline".into(),
                        },
                    ],
                },
                WindowSnapshot {
                    active: None,
                    tabs: vec![TabSnapshot {
                        url: "about:blank".into(),
                        title: String::new(),
                    }],
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_text() {
        let snap = sample();
        let encoded = snap.encode();
        let decoded = SessionSnapshot::decode(&encoded).unwrap();
        assert_eq!(snap, decoded);
    }

    #[test]
    fn rejects_foreign_and_future_files() {
        assert_eq!(
            SessionSnapshot::decode("something else\n"),
            Err(SessionError::MissingHeader)
        );
        assert_eq!(
            SessionSnapshot::decode("cinder-session\t99\n"),
            Err(SessionError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_tab_before_window() {
        let bad = "cinder-session\t1\ntab\thttps%3A%2F%2Fx\tT\n";
        assert!(matches!(
            SessionSnapshot::decode(bad),
            Err(SessionError::Malformed(_))
        ));
    }
}
