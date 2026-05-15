//! bui-url — WHATWG-shaped URL parser for the http(s) subset.
//!
//! Phase 1: handles `http://` and `https://` absolute URLs plus relative
//! resolution against an absolute base. Skipped for now: userinfo, IPv6
//! literal hosts, percent-encoding normalisation, IDNA/Punycode. These come
//! back in Phase 1+ as we encounter sites that need them.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty input")]
    Empty,
    #[error("missing scheme")]
    MissingScheme,
    #[error("unsupported scheme: {0}")]
    UnsupportedScheme(String),
    #[error("missing `//` after scheme")]
    MissingAuthority,
    #[error("empty host")]
    EmptyHost,
    #[error("invalid port: {0}")]
    InvalidPort(String),
    #[error("relative URL with no base")]
    RelativeWithoutBase,
}

impl Url {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::parse_inner(input.trim(), None)
    }

    pub fn join(&self, reference: &str) -> Result<Self, ParseError> {
        Self::parse_inner(reference.trim(), Some(self))
    }

    pub fn default_port(&self) -> u16 {
        match self.scheme.as_str() {
            "https" => 443,
            "http" => 80,
            _ => 0, // copper:// has no real port — internal-only scheme
        }
    }

    /// True for any non-network scheme that the binary handles internally
    /// (today: only `copper://` for the start page and other built-ins).
    pub fn is_internal(&self) -> bool {
        self.scheme == "copper"
    }

    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.default_port())
    }

    /// Path + query, suitable for use as the request-target in an HTTP request line.
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{}", self.path, q),
            None => self.path.clone(),
        }
    }

    /// `host[:port]` for the HTTP `Host` header. Omits the port when it
    /// matches the scheme's default.
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) if p != self.default_port() => format!("{}:{}", self.host, p),
            _ => self.host.clone(),
        }
    }

    fn parse_inner(input: &str, base: Option<&Url>) -> Result<Self, ParseError> {
        if input.is_empty() {
            return Err(ParseError::Empty);
        }

        // Detect scheme: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"
        if let Some(end) = scheme_end(input) {
            let scheme = input[..end].to_ascii_lowercase();
            let rest = &input[end + 1..];
            if scheme != "http" && scheme != "https" && scheme != "copper" {
                return Err(ParseError::UnsupportedScheme(scheme));
            }
            return parse_after_scheme(&scheme, rest);
        }

        // No scheme — must be a relative reference.
        let base = base.ok_or(ParseError::RelativeWithoutBase)?;
        resolve_relative(base, input)
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.host)?;
        if let Some(p) = self.port {
            if p != self.default_port() {
                write!(f, ":{p}")?;
            }
        }
        f.write_str(&self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        if let Some(fr) = &self.fragment {
            write!(f, "#{fr}")?;
        }
        Ok(())
    }
}

fn scheme_end(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if b == b':' {
            return Some(i);
        }
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return None;
        }
    }
    None
}

fn parse_after_scheme(scheme: &str, after_colon: &str) -> Result<Url, ParseError> {
    // Expect "//" introducing the authority.
    let rest = after_colon
        .strip_prefix("//")
        .ok_or(ParseError::MissingAuthority)?;

    // Skip userinfo if present (everything up to the *last* '@' before path/query/fragment).
    let authority_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let after_authority = &rest[authority_end..];

    let host_port = match authority.rfind('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };

    let (host, port) = split_host_port(host_port)?;
    if host.is_empty() {
        return Err(ParseError::EmptyHost);
    }

    let (path, query, fragment) = split_path_query_fragment(after_authority);

    Ok(Url {
        scheme: scheme.to_string(),
        host: host.to_ascii_lowercase(),
        port,
        path,
        query,
        fragment,
    })
}

fn split_host_port(host_port: &str) -> Result<(String, Option<u16>), ParseError> {
    // IPv6 literal: [::1]:8080
    if host_port.starts_with('[') {
        let close = host_port
            .find(']')
            .ok_or(ParseError::InvalidPort(host_port.to_string()))?;
        let host = &host_port[..=close];
        let after = &host_port[close + 1..];
        let port = parse_port_suffix(after)?;
        return Ok((host.to_string(), port));
    }
    match host_port.rfind(':') {
        Some(idx) => {
            let host = &host_port[..idx];
            let port_str = &host_port[idx + 1..];
            let port = if port_str.is_empty() {
                None
            } else {
                Some(
                    port_str
                        .parse()
                        .map_err(|_| ParseError::InvalidPort(port_str.to_string()))?,
                )
            };
            Ok((host.to_string(), port))
        }
        None => Ok((host_port.to_string(), None)),
    }
}

fn parse_port_suffix(s: &str) -> Result<Option<u16>, ParseError> {
    if s.is_empty() {
        return Ok(None);
    }
    let s = s
        .strip_prefix(':')
        .ok_or(ParseError::InvalidPort(s.to_string()))?;
    if s.is_empty() {
        return Ok(None);
    }
    s.parse()
        .map(Some)
        .map_err(|_| ParseError::InvalidPort(s.to_string()))
}

fn split_path_query_fragment(s: &str) -> (String, Option<String>, Option<String>) {
    if s.is_empty() {
        return ("/".to_string(), None, None);
    }
    let (no_frag, fragment) = match s.find('#') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    };
    let (path, query) = match no_frag.find('?') {
        Some(i) => (&no_frag[..i], Some(no_frag[i + 1..].to_string())),
        None => (no_frag, None),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    (path, query, fragment)
}

fn resolve_relative(base: &Url, reference: &str) -> Result<Url, ParseError> {
    // `//host/path` — protocol-relative
    if let Some(rest) = reference.strip_prefix("//") {
        return parse_after_scheme(&base.scheme, &format!("//{rest}"));
    }

    // `#frag`
    if let Some(frag) = reference.strip_prefix('#') {
        let mut out = base.clone();
        out.fragment = Some(frag.to_string());
        return Ok(out);
    }

    // `?query` — replaces query, drops fragment
    if let Some(q) = reference.strip_prefix('?') {
        let (q, frag) = match q.find('#') {
            Some(i) => (q[..i].to_string(), Some(q[i + 1..].to_string())),
            None => (q.to_string(), None),
        };
        let mut out = base.clone();
        out.query = Some(q);
        out.fragment = frag;
        return Ok(out);
    }

    // `/abs/path`
    if reference.starts_with('/') {
        let (path, query, fragment) = split_path_query_fragment(reference);
        return Ok(Url {
            scheme: base.scheme.clone(),
            host: base.host.clone(),
            port: base.port,
            path,
            query,
            fragment,
        });
    }

    // Relative path — resolve against base's directory.
    let base_dir = match base.path.rfind('/') {
        Some(i) => &base.path[..=i],
        None => "/",
    };
    let merged = format!("{base_dir}{reference}");
    let (path, query, fragment) = split_path_query_fragment(&merged);
    Ok(Url {
        scheme: base.scheme.clone(),
        host: base.host.clone(),
        port: base.port,
        path: normalize_dot_segments(&path),
        query,
        fragment,
    })
}

fn normalize_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {
                // Preserve leading slash by pushing an empty segment for the very first split.
                if out.is_empty() {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s),
        }
    }
    let trailing_slash = path.ends_with('/') && !path.is_empty();
    let mut joined = out.join("/");
    if trailing_slash && !joined.ends_with('/') {
        joined.push('/');
    }
    if joined.is_empty() {
        joined.push('/');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_https() {
        let u = Url::parse("https://example.com").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, None);
        assert_eq!(u.path, "/");
        assert_eq!(u.effective_port(), 443);
    }

    #[test]
    fn full() {
        let u = Url::parse("https://Example.COM:8443/a/b?x=1&y=2#frag").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("x=1&y=2"));
        assert_eq!(u.fragment.as_deref(), Some("frag"));
        assert_eq!(u.request_target(), "/a/b?x=1&y=2");
        assert_eq!(u.authority(), "example.com:8443");
    }

    #[test]
    fn http_default_port() {
        let u = Url::parse("http://example.com:80/").unwrap();
        assert_eq!(u.authority(), "example.com");
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = Url::parse("ftp://example.com/").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedScheme(_)));
    }

    #[test]
    fn accepts_copper_scheme() {
        let u = Url::parse("copper://start").unwrap();
        assert_eq!(u.scheme, "copper");
        assert_eq!(u.host, "start");
        assert!(u.is_internal());
    }

    #[test]
    fn join_root_relative() {
        let base = Url::parse("https://example.com/a/b").unwrap();
        let joined = base.join("/c/d").unwrap();
        assert_eq!(joined.path, "/c/d");
        assert_eq!(joined.host, "example.com");
    }

    #[test]
    fn join_absolute() {
        let base = Url::parse("https://example.com/a").unwrap();
        let joined = base.join("https://other.org/x").unwrap();
        assert_eq!(joined.host, "other.org");
        assert_eq!(joined.path, "/x");
    }

    #[test]
    fn join_relative() {
        let base = Url::parse("https://example.com/a/b/c").unwrap();
        let joined = base.join("../d").unwrap();
        assert_eq!(joined.path, "/a/d");
    }

    #[test]
    fn join_query_only() {
        let base = Url::parse("https://example.com/a?old=1#frag").unwrap();
        let joined = base.join("?new=2").unwrap();
        assert_eq!(joined.path, "/a");
        assert_eq!(joined.query.as_deref(), Some("new=2"));
        assert_eq!(joined.fragment, None);
    }

    #[test]
    fn display_round_trips() {
        let raw = "https://example.com:8443/a?b=c#d";
        let u = Url::parse(raw).unwrap();
        assert_eq!(u.to_string(), raw);
    }
}
