//! Cookie jar — RFC 6265 subset.
//!
//! Phase 10 implementation:
//!   * Parse `Set-Cookie` headers (name, value, Domain, Path, Expires,
//!     Max-Age, Secure, HttpOnly, SameSite).
//!   * Domain match per RFC 6265 §5.1.3, path match per §5.1.4.
//!   * `outgoing_cookies(url)` returns the cookies a request to `url` should
//!     include.
//!   * Eviction on Max-Age=0 / Expires-in-past per §5.3 step 11.
//!
//! Out of scope here: Public Suffix List filtering, HttpOnly enforcement
//! against JS (we don't yet expose `document.cookie`), partitioning per
//! storage spec.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use bui_url::Url;

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    pub expires: Option<SystemTime>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    /// Keyed by (domain, path, name) — RFC 6265 says the triple identifies a cookie.
    cookies: HashMap<(String, String, String), Cookie>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Process one `Set-Cookie` header in the context of a response from `request_url`.
    pub fn store(&mut self, set_cookie: &str, request_url: &Url) {
        let cookie = match parse_set_cookie(set_cookie, request_url) {
            Some(c) => c,
            None => return,
        };
        let key = (cookie.domain.clone(), cookie.path.clone(), cookie.name.clone());
        if let Some(expires) = cookie.expires {
            if expires <= SystemTime::now() {
                self.cookies.remove(&key);
                return;
            }
        }
        self.cookies.insert(key, cookie);
    }

    /// Iterate the cookies that should be sent with a request to `url`.
    pub fn outgoing_cookies(&self, url: &Url) -> Vec<&Cookie> {
        let now = SystemTime::now();
        let mut out: Vec<&Cookie> = self
            .cookies
            .values()
            .filter(|c| {
                if let Some(exp) = c.expires {
                    if exp <= now {
                        return false;
                    }
                }
                if c.secure && url.scheme != "https" {
                    return false;
                }
                if !domain_matches(&url.host, &c.domain, c.host_only) {
                    return false;
                }
                path_matches(&url.path, &c.path)
            })
            .collect();
        // RFC 6265: longer paths come first, then earlier creation. We don't
        // track creation time; longer path is the meaningful tiebreak.
        out.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        out
    }

    /// Render the `Cookie` header value for a request, or `None` if no cookies apply.
    pub fn cookie_header(&self, url: &Url) -> Option<String> {
        let cookies = self.outgoing_cookies(url);
        if cookies.is_empty() {
            return None;
        }
        Some(
            cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

fn parse_set_cookie(input: &str, request_url: &Url) -> Option<Cookie> {
    let mut parts = input.split(';');
    let first = parts.next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut domain: Option<String> = None;
    let mut path: Option<String> = None;
    let mut expires: Option<SystemTime> = None;
    let mut max_age: Option<i64> = None;
    let mut secure = false;
    let mut http_only = false;
    let mut same_site = SameSite::Lax;

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (attr, ""),
        };
        match k.to_ascii_lowercase().as_str() {
            "domain" => {
                let d = v.trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    domain = Some(d);
                }
            }
            "path" => path = Some(v.to_string()),
            "expires" => expires = parse_http_date(v),
            "max-age" => max_age = v.parse().ok(),
            "secure" => secure = true,
            "httponly" => http_only = true,
            "samesite" => {
                same_site = match v.to_ascii_lowercase().as_str() {
                    "strict" => SameSite::Strict,
                    "none" => SameSite::None,
                    _ => SameSite::Lax,
                };
            }
            _ => {}
        }
    }

    // Max-Age wins over Expires when both are given.
    let final_expires = if let Some(secs) = max_age {
        if secs <= 0 {
            Some(SystemTime::UNIX_EPOCH)
        } else {
            Some(SystemTime::now() + Duration::from_secs(secs as u64))
        }
    } else {
        expires
    };

    let host_only = domain.is_none();
    let cookie_domain = domain.unwrap_or_else(|| request_url.host.clone());
    let cookie_path = path.unwrap_or_else(|| default_path(&request_url.path));

    Some(Cookie {
        name,
        value,
        domain: cookie_domain,
        host_only,
        path: cookie_path,
        expires: final_expires,
        secure,
        http_only,
        same_site,
    })
}

/// Default-path algorithm per RFC 6265 §5.1.4.
fn default_path(uri_path: &str) -> String {
    if uri_path.is_empty() || !uri_path.starts_with('/') {
        return "/".into();
    }
    if uri_path == "/" {
        return "/".into();
    }
    if let Some(idx) = uri_path[1..].rfind('/') {
        let cut = idx + 1;
        if cut == 0 {
            "/".into()
        } else {
            uri_path[..cut + 1].trim_end_matches('/').to_string()
        }
    } else {
        "/".into()
    }
}

fn domain_matches(request_host: &str, cookie_domain: &str, host_only: bool) -> bool {
    let req = request_host.to_ascii_lowercase();
    let cd = cookie_domain.to_ascii_lowercase();
    if host_only {
        return req == cd;
    }
    if req == cd {
        return true;
    }
    if req.ends_with(&cd) {
        let prefix_len = req.len() - cd.len();
        if req.as_bytes()[prefix_len.saturating_sub(1)] == b'.' {
            return true;
        }
    }
    false
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if request_path.starts_with(cookie_path) {
        if cookie_path.ends_with('/') {
            return true;
        }
        if request_path.len() > cookie_path.len()
            && request_path.as_bytes()[cookie_path.len()] == b'/'
        {
            return true;
        }
    }
    false
}

fn parse_http_date(s: &str) -> Option<SystemTime> {
    // Accept the IMF-fixdate / RFC 850 / asctime formats by being permissive:
    // we only need year/month/day/hour/minute/second from any of them.
    // Format: "Wed, 21 Oct 2026 07:28:00 GMT"
    let trimmed = s.trim_end_matches(" GMT").trim();
    let parts: Vec<&str> = trimmed
        .split(|c: char| c == ' ' || c == '-')
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 5 {
        return None;
    }
    // parts[0] is the weekday + comma; parts[1..5] are day, month, year, time.
    let (day, month, year, time) = (parts[1], parts[2], parts[3], parts[4]);
    let day: u32 = day.parse().ok()?;
    let month = month_number(month)?;
    let year: i32 = year.parse().ok()?;
    let mut tparts = time.split(':');
    let hour: u32 = tparts.next()?.parse().ok()?;
    let minute: u32 = tparts.next()?.parse().ok()?;
    let second: u32 = tparts.next()?.parse().ok()?;
    let unix = days_since_epoch(year, month, day) * 86400
        + hour as i64 * 3600
        + minute as i64 * 60
        + second as i64;
    if unix < 0 {
        return Some(SystemTime::UNIX_EPOCH);
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(unix as u64))
}

fn month_number(m: &str) -> Option<u32> {
    Some(match m.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

fn days_since_epoch(year: i32, month: u32, day: u32) -> i64 {
    // Howard Hinnant's "date" algorithm, civil_from_days inverse.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146097 + doe as i64 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn round_trip_simple() {
        let mut jar = CookieJar::new();
        jar.store("sid=abc", &url("https://example.com/"));
        let header = jar.cookie_header(&url("https://example.com/")).unwrap();
        assert_eq!(header, "sid=abc");
    }

    #[test]
    fn host_only_does_not_match_subdomain() {
        let mut jar = CookieJar::new();
        jar.store("sid=abc", &url("https://example.com/"));
        assert!(jar.cookie_header(&url("https://www.example.com/")).is_none());
    }

    #[test]
    fn explicit_domain_matches_subdomain() {
        let mut jar = CookieJar::new();
        jar.store("sid=abc; Domain=example.com", &url("https://www.example.com/"));
        assert!(jar.cookie_header(&url("https://api.example.com/")).is_some());
    }

    #[test]
    fn secure_blocks_http() {
        let mut jar = CookieJar::new();
        jar.store("sid=abc; Secure", &url("https://example.com/"));
        assert!(jar.cookie_header(&url("http://example.com/")).is_none());
        assert!(jar.cookie_header(&url("https://example.com/")).is_some());
    }

    #[test]
    fn path_match() {
        let mut jar = CookieJar::new();
        jar.store("a=1; Path=/api", &url("https://example.com/"));
        assert!(jar.cookie_header(&url("https://example.com/api/x")).is_some());
        assert!(jar.cookie_header(&url("https://example.com/other")).is_none());
    }

    #[test]
    fn max_age_zero_evicts() {
        let mut jar = CookieJar::new();
        jar.store("sid=abc", &url("https://example.com/"));
        assert_eq!(jar.len(), 1);
        jar.store("sid=abc; Max-Age=0", &url("https://example.com/"));
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn longer_path_first() {
        let mut jar = CookieJar::new();
        jar.store("a=root; Path=/", &url("https://example.com/"));
        jar.store("a=api; Path=/api", &url("https://example.com/"));
        let header = jar.cookie_header(&url("https://example.com/api/x")).unwrap();
        assert!(header.starts_with("a=api"), "got {header}");
    }

    #[test]
    fn parses_expires_date() {
        let d = parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT").unwrap();
        let secs = d.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // Sanity: between 2026-01-01 and 2027-01-01.
        assert!(secs > 1_767_225_600 && secs < 1_798_761_600, "{secs}");
    }
}
