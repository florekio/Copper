use std::io::Write as _;

use bui_url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self {
            method: Method::Get,
            url,
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(self.method.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.url.request_target().as_bytes());
        out.extend_from_slice(b" HTTP/1.1\r\n");

        // Host first, always.
        let _ = write!(out, "Host: {}\r\n", self.url.authority());

        let mut has_ua = false;
        let mut has_accept = false;
        let mut has_accept_encoding = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("host") {
                continue;
            }
            if k.eq_ignore_ascii_case("user-agent") {
                has_ua = true;
            }
            if k.eq_ignore_ascii_case("accept") {
                has_accept = true;
            }
            if k.eq_ignore_ascii_case("accept-encoding") {
                has_accept_encoding = true;
            }
            let _ = write!(out, "{k}: {v}\r\n");
        }

        if !has_ua {
            out.extend_from_slice(b"User-Agent: bui/0.1\r\n");
        }
        if !has_accept {
            out.extend_from_slice(b"Accept: */*\r\n");
        }
        if !has_accept_encoding {
            // gzip only — it's what Response::decode_content understands.
            // Typically shrinks HTML/CSS/JS transfers 3-5x.
            out.extend_from_slice(b"Accept-Encoding: gzip\r\n");
        }
        // No Connection default: HTTP/1.1 connections are persistent
        // unless a side says otherwise. The Client adds `close` itself
        // for requests it won't pool (e.g. POST).

        if let Some(body) = &self.body {
            let _ = write!(out, "Content-Length: {}\r\n", body.len());
        }
        out.extend_from_slice(b"\r\n");
        if let Some(body) = &self.body {
            out.extend_from_slice(body);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_get() {
        let url = Url::parse("https://example.com/foo?bar=1").unwrap();
        let bytes = Request::get(url).serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("GET /foo?bar=1 HTTP/1.1\r\n"), "{s}");
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("Accept-Encoding: gzip\r\n"));
        // Persistent by default (HTTP/1.1) — no Connection header.
        assert!(!s.to_ascii_lowercase().contains("connection:"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn user_supplied_headers_override_defaults() {
        let url = Url::parse("https://example.com/").unwrap();
        let bytes = Request::get(url)
            .header("User-Agent", "custom/1.0")
            .header("Accept", "text/html")
            .serialize();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("User-Agent: custom/1.0\r\n"));
        assert!(s.contains("Accept: text/html\r\n"));
        // No duplicate UA
        assert_eq!(s.matches("User-Agent:").count(), 1);
        assert_eq!(s.matches("Accept:").count(), 1);
    }
}
