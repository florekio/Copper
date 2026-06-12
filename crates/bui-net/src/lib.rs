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

use std::collections::HashMap;
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

/// How long an idle pooled connection stays usable. Most origin
/// servers / CDNs keep idle HTTP/1.1 connections open for 5-60s;
/// a stale one just costs us one failed write and a retry.
const POOL_IDLE_TTL: Duration = Duration::from_secs(30);
/// Cap of idle connections kept per (host, port, tls) key.
const POOL_MAX_IDLE_PER_KEY: usize = 4;

/// A connected stream, plain or TLS, with its read buffer. The
/// BufReader stays with the connection across requests so no buffered
/// bytes are lost when it returns to the pool.
enum PooledStream {
    Plain(BufReader<TcpStream>),
    Tls(BufReader<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl PooledStream {
    async fn send_request(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            PooledStream::Plain(s) => {
                s.write_all(bytes).await?;
                s.flush().await
            }
            PooledStream::Tls(s) => {
                s.write_all(bytes).await?;
                s.flush().await
            }
        }
    }

    async fn read_response(&mut self) -> Result<(Response, bool), NetError> {
        match self {
            PooledStream::Plain(s) => Response::read_from_framed(s).await,
            PooledStream::Tls(s) => Response::read_from_framed(s).await,
        }
    }
}

struct IdleConn {
    stream: PooledStream,
    since: std::time::Instant,
}

type PoolKey = (String, u16, bool); // host, port, tls

pub struct Client {
    tls: Arc<rustls::ClientConfig>,
    pub user_agent: String,
    pub max_redirects: u8,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Per-process cookie jar. Reads happen on every request; writes
    /// happen on every Set-Cookie response.
    jar: Arc<Mutex<CookieJar>>,
    /// Idle keep-alive connections by (host, port, tls). Sync mutex,
    /// never held across an await.
    pool: Arc<Mutex<HashMap<PoolKey, Vec<IdleConn>>>>,
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
            pool: Arc::new(Mutex::new(HashMap::new())),
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
        let tls = req.url.scheme == "https";
        let key: PoolKey = (host.clone(), port, tls);
        // Only idempotent requests ride pooled connections: a stale
        // keep-alive socket fails after the request was written, and
        // re-sending a POST could double-submit.
        let poolable = matches!(req.method, Method::Get | Method::Head)
            && !req.has_header("connection");
        let request_bytes = if poolable || req.has_header("connection") {
            req.serialize()
        } else {
            req.clone().header("Connection", "close").serialize()
        };

        // Reuse an idle connection when we have one. Failure here just
        // means the server hung up while it sat in the pool — discard
        // and fall through to a fresh connection.
        if poolable {
            while let Some(mut stream) = self.take_idle(&key) {
                match self.roundtrip(&mut stream, &request_bytes).await {
                    Ok((resp, framed)) => {
                        if framed && response_keeps_alive(&resp) {
                            self.put_idle(&key, stream);
                        }
                        return Ok(resp);
                    }
                    Err(_) => continue,
                }
            }
        }

        let mut stream = self.connect(&host, port, tls).await?;
        let (resp, framed) = self.roundtrip(&mut stream, &request_bytes).await?;
        if poolable && framed && response_keeps_alive(&resp) {
            self.put_idle(&key, stream);
        }
        Ok(resp)
    }

    async fn connect(&self, host: &str, port: u16, tls: bool) -> Result<PooledStream, NetError> {
        let tcp = timeout(self.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| NetError::ConnectTimeout)??;
        let _ = tcp.set_nodelay(true);
        if tls {
            let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
            let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|_| NetError::InvalidServerName(host.to_string()))?;
            let stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| NetError::Tls(e.to_string()))?;
            Ok(PooledStream::Tls(BufReader::new(stream)))
        } else {
            Ok(PooledStream::Plain(BufReader::new(tcp)))
        }
    }

    async fn roundtrip(
        &self,
        stream: &mut PooledStream,
        request_bytes: &[u8],
    ) -> Result<(Response, bool), NetError> {
        stream.send_request(request_bytes).await?;
        timeout(self.read_timeout, stream.read_response())
            .await
            .map_err(|_| NetError::ReadTimeout)?
    }

    fn take_idle(&self, key: &PoolKey) -> Option<PooledStream> {
        let mut pool = self.pool.lock().ok()?;
        let conns = pool.get_mut(key)?;
        conns.retain(|c| c.since.elapsed() < POOL_IDLE_TTL);
        // Most recently parked first — it's the least likely to be stale.
        conns.pop().map(|c| c.stream)
    }

    fn put_idle(&self, key: &PoolKey, stream: PooledStream) {
        let Ok(mut pool) = self.pool.lock() else { return };
        let conns = pool.entry(key.clone()).or_default();
        conns.push(IdleConn {
            stream,
            since: std::time::Instant::now(),
        });
        if conns.len() > POOL_MAX_IDLE_PER_KEY {
            conns.remove(0); // drop the oldest
        }
    }
}

/// Whether the server allows reusing the connection. No Connection
/// header means persistent (the HTTP/1.1 default); an HTTP/1.0 server
/// that closes anyway just costs the next request one stale-retry.
fn response_keeps_alive(resp: &Response) -> bool {
    match resp.header("connection") {
        None => true,
        Some(v) => !v
            .split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("close")),
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
