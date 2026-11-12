//! Input sanitizers for untrusted engine data (spec §10): URLs, titles,
//! download filenames. Everything that crosses the boundary is validated and
//! length-limited here before it becomes product state.

use std::fmt;

pub const MAX_URL_LEN: usize = 8 * 1024;
pub const MAX_TITLE_CHARS: usize = 512;
pub const MAX_FILENAME_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    Empty,
    TooLong,
    ControlCharacter,
    BadScheme,
    MissingHost,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty URL"),
            Self::TooLong => f.write_str("URL too long"),
            Self::ControlCharacter => f.write_str("URL contains control characters or whitespace"),
            Self::BadScheme => f.write_str("URL scheme is not allowed"),
            Self::MissingHost => f.write_str("URL has no host"),
        }
    }
}

impl std::error::Error for UrlError {}

/// A validated URL. Only schemes Cinder will hand to the engine are accepted;
/// `javascript:` and `data:` are rejected for navigation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Url(String);

impl Url {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(UrlError::Empty);
        }
        if trimmed.len() > MAX_URL_LEN {
            return Err(UrlError::TooLong);
        }
        if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(UrlError::ControlCharacter);
        }

        let (scheme, rest) = trimmed.split_once(':').ok_or(UrlError::BadScheme)?;
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "about" | "file") {
            return Err(UrlError::BadScheme);
        }
        if rest.is_empty() {
            return Err(UrlError::MissingHost);
        }

        match scheme.as_str() {
            "http" | "https" => {
                let authority = rest.strip_prefix("//").ok_or(UrlError::MissingHost)?;
                if host_of(authority).is_none() {
                    return Err(UrlError::MissingHost);
                }
            }
            "file" if !rest.starts_with("//") && !rest.starts_with('/') => {
                return Err(UrlError::MissingHost)
            }
            _ => {}
        }

        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> &str {
        self.0.split_once(':').map(|(s, _)| s).unwrap_or("")
    }

    /// Lowercased host, without userinfo or port. `None` for non-authority URLs.
    pub fn host(&self) -> Option<String> {
        let (_, rest) = self.0.split_once(':')?;
        let authority = rest.strip_prefix("//")?;
        host_of(authority)
    }

    /// `scheme://host[:port]`, used for permission storage keys.
    pub fn origin(&self) -> String {
        match self.host() {
            Some(host) => format!("{}://{}", self.scheme(), host),
            None => format!("{}:", self.scheme()),
        }
    }
}

fn host_of(authority: &str) -> Option<String> {
    let end = authority
        .find(['/', '?', '#'])
        .unwrap_or(authority.len());
    let authority = &authority[..end];
    let after_userinfo = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = match after_userinfo.rfind(':') {
        Some(i) if after_userinfo[i + 1..].chars().all(|c| c.is_ascii_digit()) => &after_userinfo[..i],
        _ => after_userinfo,
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// What the omnibox input meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmniboxInput {
    Url(Url),
    Search(String),
}

/// Resolve omnibox text to a URL or a search term. Anything that is not
/// clearly a URL becomes a search, never a navigation.
pub fn classify_omnibox(input: &str) -> Result<OmniboxInput, UrlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(UrlError::Empty);
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(UrlError::TooLong);
    }

    let looks_like_url = trimmed.contains("://")
        || trimmed.starts_with("about:")
        || (!trimmed.contains(char::is_whitespace)
            && (trimmed.contains('.') || trimmed.starts_with("localhost")));

    if looks_like_url {
        if trimmed.contains("://") || trimmed.starts_with("about:") {
            return Url::parse(trimmed).map(OmniboxInput::Url);
        }
        return Url::parse(&format!("https://{trimmed}")).map(OmniboxInput::Url);
    }

    let mut search: String = trimmed.chars().take(MAX_TITLE_CHARS).collect();
    search.shrink_to_fit();
    Ok(OmniboxInput::Search(search))
}

/// Make an engine-supplied download name safe for the filesystem.
pub fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect::<String>();

    let base = base.trim().trim_end_matches('.').trim();
    if base.is_empty() || base == "." || base == ".." {
        return "download".to_string();
    }

    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 && base.len() - i <= 17 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };

    if is_reserved_windows_name(stem) {
        return format!("_{stem}{ext}");
    }

    let total = base.chars().count();
    if total <= MAX_FILENAME_CHARS {
        return base.to_string();
    }

    let ext_chars = ext.chars().count();
    let keep = MAX_FILENAME_CHARS.saturating_sub(ext_chars);
    let truncated: String = stem.chars().take(keep).collect();
    format!("{truncated}{ext}")
}

fn is_reserved_windows_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit())
}

/// Titles come from the page: strip control characters and cap the length.
pub fn sanitize_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect();
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_https_about_file() {
        for ok in [
            "https://example.com",
            "http://example.com:8080/a?b=c#d",
            "about:blank",
            "file:///tmp/x.html",
        ] {
            assert!(Url::parse(ok).is_ok(), "{ok} should parse");
        }
    }

    #[test]
    fn rejects_dangerous_or_malformed_schemes() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>",
            "https://",
            "https://exa mple.com",
            "",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn extracts_host_and_origin() {
        let u = Url::parse("https://User:pw@Example.COM:8443/path").unwrap();
        assert_eq!(u.host().as_deref(), Some("example.com"));
        assert_eq!(u.origin(), "https://example.com");
        assert_eq!(u.scheme(), "https");
    }

    #[test]
    fn omnibox_separates_urls_from_searches() {
        assert_eq!(
            classify_omnibox("example.com").unwrap(),
            OmniboxInput::Url(Url::parse("https://example.com").unwrap())
        );
        assert_eq!(
            classify_omnibox("how do tabs work").unwrap(),
            OmniboxInput::Search("how do tabs work".into())
        );
        assert!(classify_omnibox("   ").is_err());
    }

    #[test]
    fn filenames_cannot_escape_the_download_directory() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\windows\\system32"), "system32");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn long_filenames_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(400));
        let out = sanitize_filename(&long);
        assert!(out.chars().count() <= MAX_FILENAME_CHARS);
        assert!(out.ends_with(".pdf"));
    }

    #[test]
    fn titles_are_stripped_and_capped() {
        assert_eq!(sanitize_title("  hi\u{0}there  "), "hi there");
        assert_eq!(sanitize_title(&"x".repeat(2000)).chars().count(), MAX_TITLE_CHARS);
    }
}
