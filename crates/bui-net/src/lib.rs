//! bui-net — HTTP/1.1 client over tokio + rustls.
//!
//! Phase 1: hand-rolled request serializer + response parser, supports
//! `Content-Length` and `Transfer-Encoding: chunked` body framing, and
//! follows redirects up to a configurable cap. No connection pooling, no
//! HTTP/2, no compression. Cookie jar is wired into Client (RFC 6265
//! subset) — sessions survive within a process run but aren't persisted
//! to disk yet.

mod cookie;
mod request;
mod response;

pub use bui_url::{ParseError as UrlParseError, Url};
pub use cookie::{Cookie, CookieJar, SameSite};
pub use request::{Method, Request};
pub use response::Response;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("invalid server name: {0}")]
    InvalidServerName(String),
    #[error("malformed status line: {0}")]
    MalformedStatusLine(String),
    #[error("malformed header: {0}")]
    MalformedHeader(String),
    #[error("malformed chunk: {0}")]
    MalformedChunk(String),
    #[error("redirect without Location header (status {0})")]
    RedirectMissingLocation(u16),
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("redirect target: {0}")]
    BadRedirectTarget(#[from] UrlParseError),
    #[error("connect timeout")]
    ConnectTimeout,
    #[error("read timeout")]
    ReadTimeout,
}

pub struct Client {
    tls: Arc<rustls::ClientConfig>,
    pub user_agent: String,
    pub max_redirects: u8,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Per-process cookie jar. Reads happen on every request; writes
    /// happen on every Set-Cookie response.
    jar: Arc<Mutex<CookieJar>>,
}

impl Client {
    pub fn new() -> Self {
        Self {
            tls: tls_config().clone(),
            // Browser-shaped User-Agent. Wikimedia and a number of
            // CDNs aggressively rate-limit (or outright block)
            // unfamiliar UAs — `bui/0.1.0` was getting steady HTTP
            // 429s on Wikipedia article images. Identifying as a
            // real-looking browser unblocks the typical browse path
            // without lying about what the network sees from us.
            user_agent: format!(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Copper/{} Safari/537.36",
                env!("CARGO_PKG_VERSION"),
            ),
            max_redirects: 10,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(60),
            jar: Arc::new(Mutex::new(CookieJar::new())),
        }
    }

    /// Borrow the cookie jar — useful for tests / debugging.
    pub fn jar(&self) -> Arc<Mutex<CookieJar>> {
        self.jar.clone()
    }

    pub async fn get(&self, url: &Url) -> Result<Response, NetError> {
        self.send(Request::get(url.clone())).await
    }

    pub async fn send(&self, mut req: Request) -> Result<Response, NetError> {
        for _ in 0..=self.max_redirects {
            if !req.has_header("user-agent") {
                req = req.header("User-Agent", self.user_agent.clone());
            }
            // Drop any stale Cookie header from a previous redirect hop;
            // the jar gets the final say for the current URL.
            req.headers
                .retain(|(k, _)| !k.eq_ignore_ascii_case("cookie"));
            if let Ok(jar) = self.jar.lock() {
                if let Some(cookies) = jar.cookie_header(&req.url) {
                    req = req.header("Cookie", cookies);
                }
            }
            let resp = self.fetch_one(&req).await?;
            // Store any Set-Cookie headers from the response. RFC 6265
            // says Set-Cookie can repeat — process them in order so
            // later headers can supersede earlier ones for the same key.
            if let Ok(mut jar) = self.jar.lock() {
                for (name, value) in &resp.headers {
                    if name.eq_ignore_ascii_case("set-cookie") {
                        jar.store(value, &req.url);
                    }
                }
            }
            if is_redirect(resp.status) {
                let location = resp
                    .header("location")
                    .ok_or(NetError::RedirectMissingLocation(resp.status))?
                    .to_string();
                let new_url = req.url.join(&location)?;
                // 303 always switches to GET; 301/302 by convention also drop body for GET requests we already issued.
                req.url = new_url;
                continue;
            }
            return Ok(resp);
        }
        Err(NetError::TooManyRedirects)
    }

    async fn fetch_one(&self, req: &Request) -> Result<Response, NetError> {
        let host = req.url.host.clone();
        let port = req.url.effective_port();
        let request_bytes = req.serialize();

        let tcp = timeout(
            self.connect_timeout,
            TcpStream::connect((host.as_str(), port)),
        )
        .await
        .map_err(|_| NetError::ConnectTimeout)??;
        let _ = tcp.set_nodelay(true);

        let result = if req.url.scheme == "https" {
            let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
            let server_name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|_| NetError::InvalidServerName(host.clone()))?;
            let mut stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| NetError::Tls(e.to_string()))?;
            stream.write_all(&request_bytes).await?;
            stream.flush().await?;
            let mut reader = BufReader::new(stream);
            timeout(self.read_timeout, Response::read_from(&mut reader))
                .await
                .map_err(|_| NetError::ReadTimeout)?
        } else {
            let mut stream = tcp;
            stream.write_all(&request_bytes).await?;
            stream.flush().await?;
            let mut reader = BufReader::new(stream);
            timeout(self.read_timeout, Response::read_from(&mut reader))
                .await
                .map_err(|_| NetError::ReadTimeout)?
        };

        result
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn tls_config() -> &'static Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = rustls::crypto::ring::default_provider();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .expect("rustls protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    })
}
