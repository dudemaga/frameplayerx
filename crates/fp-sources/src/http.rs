//! HTTP(S) client with retries, and [`HttpFile`]: a random-access
//! [`ByteSource`] over HTTP Range requests.
//!
//! - Ranges are fetched in blocks through the shared block cache, with
//!   background read-ahead when reading sequentially: one block at a time at
//!   first, then one growing request cut into blocks as it arrives.
//! - One `ureq` agent per client keeps connections alive between requests.
//! - Transient failures (connection errors, timeouts, 408/429/5xx) are
//!   retried with exponential backoff.
//! - Servers without range support fall back to one sequential stream:
//!   forward seeks skip data, backward seeks outside the cache fail with
//!   [`std::io::ErrorKind::Unsupported`].
//! - A server that stops sending fails the read after the read timeout,
//!   however long a stream has been running.
//! - TLS is rustls with the ring provider and webpki roots.

use crate::cache::{CacheOptions, CacheStats, CachedSource, RangeBody, RangeFetch};
use crate::config::Credentials;
use crate::error::{Error, Result};
use crate::urlutil::{basic_auth_value, redact, same_origin, split_credentials};
use fp_core::ByteSource;
use fp_core::sync::lock;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;
use ureq::http;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};

/// Connection, retry and caching settings.
#[derive(Clone, Debug)]
pub struct HttpOptions {
    /// Time allowed to open a TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Time allowed for response headers, for the body of a bounded request
    /// (one cache block, a listing, a feed), and between any two reads of a
    /// body: a stream with no overall deadline still fails once its server
    /// stops sending.
    pub read_timeout: Duration,
    /// Extra attempts after a transient failure.
    pub max_retries: u32,
    /// Delay before the first retry; doubles on each attempt (max 5 s).
    pub retry_delay: Duration,
    /// Accept invalid TLS certificates (self-signed NAS certificates).
    pub insecure_tls: bool,
    /// `User-Agent` header.
    pub user_agent: String,
    /// Block cache used by [`HttpFile`].
    pub cache: CacheOptions,
    /// Furthest a forward seek may skip by reading and discarding data when
    /// the server does not support ranges.
    pub max_forward_skip: u64,
}

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_millis(250),
            insecure_tls: false,
            user_agent: concat!("FramePlayerX/", env!("CARGO_PKG_VERSION")).to_string(),
            cache: CacheOptions::default(),
            max_forward_skip: 256 << 20,
        }
    }
}

/// A complete HTTP response with a bounded body.
#[derive(Clone, Debug)]
pub struct HttpReply {
    pub status: u16,
    /// Headers with lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpReply {
    /// First value of a header (name compared case-insensitively).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body as text (invalid UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Turns a non-2xx status into the matching [`Error`].
    pub fn error_for_status(self, url: &str) -> Result<HttpReply> {
        status_error(self.status, url).map_or(Ok(self), Err)
    }
}

/// Maps a status to an error; `None` for 2xx.
fn status_error(status: u16, url: &str) -> Option<Error> {
    match status {
        200..=299 => None,
        401 | 403 => Some(Error::Auth(redact(url))),
        404 | 410 => Some(Error::NotFound(redact(url))),
        _ => Some(Error::HttpStatus {
            status,
            url: redact(url),
        }),
    }
}

fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn is_transient_error(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
            | ureq::Error::Protocol(_)
    )
}

/// Outcome of one attempt.
enum Failure {
    /// Worth retrying, optionally after a server-requested delay.
    Retry(Error, Option<Duration>),
    /// Give up.
    Fatal(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Fatal(e)
    }
}

type Response = http::Response<ureq::Body>;

/// The agent's connections, each wait for the network capped at the read
/// timeout: ureq's body deadline covers a whole body, which a video stream
/// can't have, and without one a stalled server blocks its reader forever.
fn agent(opts: &HttpOptions) -> ureq::Agent {
    agent_redirects(opts, true)
}

fn agent_redirects(opts: &HttpOptions, follow_redirects: bool) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .disable_verification(opts.insecure_tls)
        .build();
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .timeout_connect(Some(opts.connect_timeout))
        .timeout_recv_response(Some(opts.read_timeout))
        .timeout_recv_body(Some(opts.read_timeout))
        .max_idle_connections_per_host(4)
        .user_agent(opts.user_agent.as_str())
        .tls_config(tls);
    let config = if follow_redirects {
        config
    } else {
        config.max_redirects(0)
    }
    .build();
    let connector = DefaultConnector::new().chain(StallLimit(opts.read_timeout));
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

/// One scoped socket alternative. The request URI remains the source of
/// the HTTP Host header and TLS server name.
#[derive(Clone, Debug)]
struct AddressFallback {
    scheme: String,
    host: String,
    port: u16,
    address: SocketAddr,
}

impl AddressFallback {
    fn applies(&self, uri: &http::Uri, config: &ureq::config::Config) -> bool {
        let port = uri.port_u16().or_else(|| match uri.scheme_str() {
            Some("http") => Some(80),
            Some("https") => Some(443),
            _ => None,
        });
        uri.scheme_str() == Some(self.scheme.as_str())
            && uri
                .host()
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            && port == Some(self.port)
            && config.proxy().is_none_or(|proxy| proxy.is_no_proxy(uri))
    }

    fn addresses(&self) -> ResolvedSocketAddrs {
        let mut addresses = DefaultResolver::default().empty();
        addresses.push(self.address);
        addresses
    }

    fn connect_error(&self, error: ureq::Error) -> ureq::Error {
        let kind = match &error {
            ureq::Error::Io(error) => error.kind(),
            ureq::Error::Timeout(_) => io::ErrorKind::TimedOut,
            _ => return error,
        };
        ureq::Error::Io(io::Error::new(
            kind,
            format!("advertised address {} failed: {error}", self.address),
        ))
    }
}

#[derive(Debug)]
struct AddressFallbackResolver {
    resolver: DefaultResolver,
    fallback: AddressFallback,
}

impl Resolver for AddressFallbackResolver {
    fn resolve(
        &self,
        uri: &http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let result = self.resolver.resolve(uri, config, timeout);
        if !self.fallback.applies(uri, config) {
            return result;
        }
        match result {
            Ok(addresses) if !addresses.is_empty() => Ok(addresses),
            Ok(_)
            | Err(ureq::Error::HostNotFound | ureq::Error::Io(_) | ureq::Error::Timeout(_)) => {
                log::debug!(
                    "DNS lookup for {}:{} failed; using advertised address {}",
                    self.fallback.host,
                    self.fallback.port,
                    self.fallback.address
                );
                Ok(self.fallback.addresses())
            }
            Err(error) => Err(error),
        }
    }
}

/// Retry connection establishment, including TLS, before any HTTP request
/// can be sent. A successful DNS lookup need not mean a reachable socket.
#[derive(Debug)]
struct AddressFallbackConnector<C> {
    inner: C,
    fallback: AddressFallback,
}

fn retryable_connect_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(ureq::Timeout::Connect) | ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::HostUnreachable
                | io::ErrorKind::NetworkUnreachable
                | io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::TimedOut
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe
        ),
        // Certificate, TLS protocol and global deadline errors are final.
        _ => false,
    }
}

impl<C: Connector<(), Out = Box<dyn Transport>>> Connector<()> for AddressFallbackConnector<C> {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        if !self.fallback.applies(details.uri, details.config) {
            return self.inner.connect(details, chained);
        }
        if details.timeout.after.is_zero() {
            return Err(ureq::Error::Timeout(details.timeout.reason));
        }
        // Keep the advertised socket for one explicit attempt, even when it
        // appeared after another DNS address whose TLS handshake can stall.
        let mut normal = details.resolver.empty();
        for address in &details.addrs {
            if *address != self.fallback.address {
                normal.push(*address);
            }
        }
        if normal.is_empty() {
            log::info!(
                "Connecting to {}:{} using advertised address {}",
                self.fallback.host,
                self.fallback.port,
                self.fallback.address
            );
            return self
                .inner
                .connect(details, chained)
                .map_err(|error| self.fallback.connect_error(error));
        }
        let now = (details.current_time)();
        let mut attempt = ConnectionDetails {
            uri: details.uri,
            addrs: normal,
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now,
            timeout: NextTimeout {
                // Reserve time for the advertised socket rather than letting
                // an unreachable DNS address consume the whole deadline.
                after: if details.timeout.reason == ureq::Timeout::Connect {
                    details.timeout.after / 2
                } else {
                    details.timeout.after
                },
                ..details.timeout
            },
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
            register_shutdown: details.register_shutdown.clone(),
        };
        match self.inner.connect(&attempt, chained) {
            Err(error) if retryable_connect_error(&error) => {
                let now = (details.current_time)();
                let remaining = if details.timeout.after.is_not_happening() {
                    details.timeout.after
                } else {
                    use ureq::unversioned::transport::time::Instant;
                    let elapsed = match (details.now, now) {
                        (Instant::Exact(start), Instant::Exact(now)) => {
                            now.saturating_duration_since(start)
                        }
                        _ => Duration::ZERO,
                    };
                    let remaining = (*details.timeout.after).saturating_sub(elapsed);
                    if remaining.is_zero() {
                        return Err(error);
                    }
                    remaining.into()
                };
                log::info!(
                    "Connection to {}:{} failed ({error}); trying advertised address {}",
                    self.fallback.host,
                    self.fallback.port,
                    self.fallback.address
                );
                attempt.addrs = self.fallback.addresses();
                attempt.now = now;
                attempt.timeout.after = remaining;
                self.inner
                    .connect(&attempt, chained)
                    .map_err(|error| self.fallback.connect_error(error))
            }
            result => result,
        }
    }
}

/// Wraps each connection in a [`Stalling`] transport.
#[derive(Debug)]
struct StallLimit(Duration);

impl Connector<Box<dyn Transport>> for StallLimit {
    type Out = Stalling;

    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<Box<dyn Transport>>,
    ) -> std::result::Result<Option<Stalling>, ureq::Error> {
        Ok(chained.map(|inner| Stalling {
            inner,
            limit: self.0,
        }))
    }
}

/// A connection whose every send and receive gives up after `limit`
/// without progress.
#[derive(Debug)]
struct Stalling {
    inner: Box<dyn Transport>,
    limit: Duration,
}

impl Stalling {
    fn capped(&self, t: NextTimeout) -> NextTimeout {
        // The rest (a newer ureq's per-read and per-write limits) is for
        // the transport doing the I/O to apply, so it passes through.
        NextTimeout {
            after: (*t.after).min(self.limit).into(),
            ..t
        }
    }
}

impl Transport for Stalling {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        let timeout = self.capped(timeout);
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let timeout = self.capped(timeout);
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// HTTP client shared by the HTTP, WebDAV, DLNA and feed sources.
///
/// Cloning is cheap and shares the connection pool.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    auth: Option<String>,
    /// The server `auth` is for: other origins a server links to (a CDN,
    /// another host's media or scripts) get no credentials.
    scope: Option<String>,
    /// Named credentials are not delegated to the HTTP library's redirect
    /// handling: an arbitrary custom token header might otherwise follow.
    headers: Vec<(String, String)>,
    opts: HttpOptions,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("auth", &self.auth.as_ref().map(|_| "***"))
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Creates a client. `credentials` are sent as HTTP Basic auth with
    /// every request; credentials embedded in a URL are used when none are
    /// given here.
    pub fn new(opts: HttpOptions, credentials: Option<&Credentials>) -> HttpClient {
        HttpClient {
            agent: agent(&opts),
            auth: credentials.map(basic_auth_value),
            scope: None,
            headers: Vec::new(),
            opts,
        }
    }

    /// Sends the configured credentials only to `url`'s origin (scheme,
    /// host and port).
    pub fn scoped_to(mut self, url: &str) -> HttpClient {
        self.scope = Some(url.to_string());
        self
    }

    /// Creates a client with a custom `Authorization` header value sent with
    /// every request to `scope`'s origin. Used by sources that need a
    /// non-Basic auth scheme (e.g. the MediaBrowser `Authorization:
    /// MediaBrowser ...` header).
    pub fn new_with_header(
        opts: HttpOptions,
        auth_header: impl Into<String>,
        scope: &str,
    ) -> HttpClient {
        HttpClient {
            agent: agent(&opts),
            auth: Some(auth_header.into()),
            scope: Some(scope.to_string()),
            headers: Vec::new(),
            opts,
        }
    }

    /// Sends named headers only to `scope`'s origin, including every range
    /// request. Same-origin redirects keep them; cross-origin redirects
    /// are refused rather than forwarding credentials or request bodies.
    pub fn new_with_headers(
        opts: HttpOptions,
        headers: Vec<(String, String)>,
        scope: &str,
    ) -> HttpClient {
        HttpClient {
            agent: agent_redirects(&opts, false),
            auth: None,
            scope: Some(scope.to_string()),
            headers,
            opts,
        }
    }

    /// Uses an advertised socket address if DNS fails or the ordinary
    /// connection cannot be established, including its TLS handshake.
    /// Other origins still use ordinary DNS, including other ports on the
    /// same host. HTTPS keeps the original hostname and certificate checks.
    /// A client without a valid HTTP(S) scope is returned unchanged.
    pub fn with_address_fallback(mut self, address: SocketAddr) -> Self {
        let Some(scope) = self
            .scope
            .as_deref()
            .and_then(|scope| url::Url::parse(scope).ok())
        else {
            return self;
        };
        if !matches!(scope.scheme(), "http" | "https") {
            return self;
        }
        let (Some(host), Some(port)) = (scope.host_str(), scope.port_or_known_default()) else {
            return self;
        };
        let fallback = AddressFallback {
            scheme: scope.scheme().into(),
            host: host.into(),
            port,
            address,
        };
        let resolver = AddressFallbackResolver {
            resolver: DefaultResolver::default(),
            fallback: fallback.clone(),
        };
        let connector = AddressFallbackConnector {
            inner: DefaultConnector::new(),
            fallback,
        }
        .chain(StallLimit(self.opts.read_timeout));
        // Reuse the complete configuration, including redirect policy and
        // TLS verification, rather than constructing alternate TLS options.
        self.agent = ureq::Agent::with_parts(self.agent.config().clone(), connector, resolver);
        self
    }

    /// The options this client was created with.
    pub fn options(&self) -> &HttpOptions {
        &self.opts
    }

    fn retrying<T>(
        &self,
        url: &str,
        mut op: impl FnMut() -> std::result::Result<T, Failure>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match op() {
                Ok(v) => return Ok(v),
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e, after)) => {
                    if attempt >= self.opts.max_retries {
                        return Err(e);
                    }
                    let backoff = self
                        .opts
                        .retry_delay
                        .saturating_mul(1 << attempt.min(8))
                        .min(Duration::from_secs(5));
                    let delay = after.unwrap_or(backoff).min(Duration::from_secs(10));
                    log::debug!("retrying {} in {delay:?} after: {e}", redact(url));
                    std::thread::sleep(delay);
                    attempt += 1;
                }
            }
        }
    }

    /// Sends one request. Transient transport errors become `Retry`.
    fn send_once(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        unbounded_body: bool,
    ) -> std::result::Result<Response, Failure> {
        self.send_redirects(method, url, headers, body, unbounded_body, 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_redirects(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        unbounded_body: bool,
        redirects: u8,
    ) -> std::result::Result<Response, Failure> {
        let (clean_url, url_creds) = split_credentials(url)?;
        let auth = self
            .auth
            .clone()
            .filter(|_| {
                self.scope
                    .as_deref()
                    .is_none_or(|s| same_origin(s, &clean_url))
            })
            .or_else(|| url_creds.as_ref().map(basic_auth_value));
        let mut builder = http::Request::builder().method(method).uri(&clean_url);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        if self
            .scope
            .as_deref()
            .is_some_and(|s| same_origin(s, &clean_url))
        {
            for (k, v) in &self.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
        }
        if let Some(a) = &auth {
            builder = builder.header("Authorization", a.as_str());
            if a.starts_with("MediaBrowser ") {
                builder = builder.header("X-Emby-Authorization", a.as_str());
                if let Some(tok) = a.split("Token=\"").nth(1).and_then(|t| t.split('"').next()) {
                    builder = builder.header("X-Emby-Token", tok);
                }
            }
        }
        let build_err = |e: http::Error| Failure::Fatal(Error::invalid(url, e.to_string()));
        let result = match body {
            Some(b) => {
                let req = builder.body(b.to_vec()).map_err(build_err)?;
                self.agent.run(req)
            }
            None => {
                let req = builder.body(()).map_err(build_err)?;
                if unbounded_body {
                    let req = self
                        .agent
                        .configure_request(req)
                        .timeout_recv_body(None)
                        .build();
                    self.agent.run(req)
                } else {
                    self.agent.run(req)
                }
            }
        };
        let response = result.map_err(|e| {
            let err = Error::Http {
                url: redact(&clean_url),
                message: e.to_string(),
            };
            if is_transient_error(&e) {
                Failure::Retry(err, None)
            } else {
                Failure::Fatal(err)
            }
        })?;
        if !self.headers.is_empty()
            && matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
        {
            let Some(location) = response
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
            else {
                return Ok(response);
            };
            let next = url::Url::parse(&clean_url)
                .and_then(|base| base.join(location))
                .map_err(|_| Failure::Fatal(Error::invalid(url, "invalid HTTP redirect")))?;
            if !same_origin(&clean_url, next.as_str()) {
                return Err(Failure::Fatal(Error::invalid(
                    url,
                    "authenticated redirect changed origin",
                )));
            }
            if !next.username().is_empty() || next.password().is_some() {
                return Err(Failure::Fatal(Error::invalid(
                    url,
                    "HTTP redirect contains credentials",
                )));
            }
            if redirects >= 10 {
                return Err(Failure::Fatal(Error::invalid(
                    url,
                    "too many HTTP redirects",
                )));
            }
            let (method, body) = if response.status().as_u16() == 303
                || (matches!(response.status().as_u16(), 301 | 302) && method == "POST")
            {
                ("GET", None)
            } else {
                (method, body)
            };
            drop(response);
            return self.send_redirects(
                method,
                next.as_str(),
                headers,
                body,
                unbounded_body,
                redirects + 1,
            );
        }
        Ok(response)
    }

    /// Sends a request and reads the whole response body (at most `limit`
    /// bytes). Transient failures (including 408/429/5xx statuses) are
    /// retried; every other status is returned as is.
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<HttpReply> {
        self.request_inner(method, url, headers, body, limit, true, |_| true)
            .map(|r| r.0)
    }

    /// Like [`HttpClient::request`], but a 5xx status is returned instead of
    /// retried: SOAP reports faults as `500` with a meaningful body.
    pub fn request_no_status_retry(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<HttpReply> {
        self.request_inner(method, url, headers, body, limit, false, |_| true)
            .map(|r| r.0)
    }

    /// Like [`HttpClient::request`], but the body is only read when
    /// `want_body` accepts the response headers (lower-case names).
    /// Otherwise the reply comes back with an empty body and `false`, and
    /// the connection is dropped without downloading anything. Used to tell
    /// a JSON document from a media stream at an unknown URL.
    pub fn request_if(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
        want_body: impl Fn(&HttpReply) -> bool,
    ) -> Result<(HttpReply, bool)> {
        self.request_inner(method, url, headers, body, limit, true, want_body)
    }

    #[allow(clippy::too_many_arguments)]
    fn request_inner(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
        retry_status: bool,
        want_body: impl Fn(&HttpReply) -> bool,
    ) -> Result<(HttpReply, bool)> {
        self.retrying(url, || {
            let resp = self.send_once(method, url, headers, body, false)?;
            let status = resp.status().as_u16();
            let retry_after = retry_after(&resp);
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_ascii_lowercase(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut reply = HttpReply {
                status,
                headers,
                body: Vec::new(),
            };
            if retry_status && is_transient_status(status) {
                return Err(Failure::Retry(
                    Error::HttpStatus {
                        status,
                        url: redact(url),
                    },
                    retry_after,
                ));
            }
            if !want_body(&reply) {
                return Ok((reply, false));
            }
            reply.body = read_body(resp.into_body().into_reader(), limit).map_err(|e| {
                Failure::Retry(
                    Error::Http {
                        url: redact(url),
                        message: format!("reading body: {e}"),
                    },
                    None,
                )
            })?;
            Ok((reply, true))
        })
    }

    /// GETs a URL and returns its body (at most `limit` bytes); non-2xx
    /// statuses are errors.
    pub fn get_bytes(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        Ok(self
            .request("GET", url, &[], None, limit)?
            .error_for_status(url)?
            .body)
    }

    /// GETs a URL as text (at most 32 MiB).
    pub fn get_text(&self, url: &str) -> Result<String> {
        let body = self.get_bytes(url, 32 << 20)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Opens a URL for random access.
    pub fn open(&self, url: &str) -> Result<HttpFile> {
        HttpFile::open(self, url)
    }
}

impl HttpClient {
    /// GETs `url` to read its body as it arrives, with no ranges and no
    /// cache: a server's live transcode, which sends no faster than it
    /// encodes. A read waits only for the next bytes the server sends.
    pub fn get_stream(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        let resp = self.retrying(url, || {
            let resp = self.send_once("GET", url, &[], None, true)?;
            let status = resp.status().as_u16();
            if is_transient_status(status) {
                return Err(Failure::Retry(
                    Error::HttpStatus {
                        status,
                        url: redact(url),
                    },
                    retry_after(&resp),
                ));
            }
            if let Some(e) = status_error(status, url) {
                return Err(Failure::Fatal(e));
            }
            Ok(resp)
        })?;
        Ok(Box::new(resp.into_body().into_reader()))
    }
}

fn retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Reads a body to its end, keeping at most `limit` bytes. Reading to EOF
/// lets the agent return the connection to its pool; when the body is
/// longer than `limit` the rest is abandoned (and the connection closed).
fn read_body(mut reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut reader).take(limit).read_to_end(&mut out)?;
    if out.len() as u64 == limit {
        // Probe for EOF so a body of exactly `limit` bytes is fully consumed.
        let mut probe = [0u8; 1];
        let _ = reader.read(&mut probe);
    }
    Ok(out)
}

/// Parsed `Content-Range` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    /// First and last byte, `None` for `bytes */total`.
    pub range: Option<(u64, u64)>,
    /// Complete length, `None` for `*`.
    pub total: Option<u64>,
}

pub(crate) fn parse_content_range(v: &str) -> Option<ContentRange> {
    let v = v.trim();
    let rest = v.strip_prefix("bytes")?.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    let range = match range.trim() {
        "*" => None,
        r => {
            let (a, b) = r.split_once('-')?;
            let (a, b) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if b < a {
                return None;
            }
            Some((a, b))
        }
    };
    Some(ContentRange { range, total })
}

struct Stream {
    reader: Box<dyn Read + Send>,
    pos: u64,
}

/// Range fetcher behind [`HttpFile`].
struct HttpFetcher {
    client: HttpClient,
    url: String,
    size: Option<u64>,
    ranges: bool,
    stream: Mutex<Option<Stream>>,
}

impl HttpFetcher {
    fn display(&self) -> String {
        redact(&self.url)
    }

    /// `len` bytes from `offset`, fewer only at the end of the file. A server
    /// that serves smaller ranges than asked is asked again for the rest.
    fn fetch_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while out.len() < len {
            let (part, capped) =
                self.fetch_range_once(offset + out.len() as u64, len - out.len())?;
            out.extend_from_slice(&part);
            if !capped || part.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// One range request: the bytes, and whether the server sent a smaller
    /// range than asked for (and has more).
    fn fetch_range_once(&self, offset: u64, len: usize) -> Result<(Vec<u8>, bool)> {
        let mut end = offset + len as u64 - 1;
        if let Some(size) = self.size {
            if offset >= size {
                return Ok((Vec::new(), false));
            }
            end = end.min(size - 1);
        }
        let range = format!("bytes={offset}-{end}");
        let want = (end - offset + 1) as usize;
        self.client.retrying(&self.url, || {
            let resp = self.client.send_once(
                "GET",
                &self.url,
                &[("Range", range.as_str())],
                None,
                false,
            )?;
            let status = resp.status().as_u16();
            let cr = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            let after = retry_after(&resp);
            let io_retry = |e: io::Error| {
                Failure::Retry(
                    Error::Http {
                        url: self.display(),
                        message: format!("reading range {range}: {e}"),
                    },
                    None,
                )
            };
            match status {
                206 => {
                    let served = cr.and_then(|c| c.range);
                    if let Some((start, _)) = served
                        && start != offset
                    {
                        return Err(Failure::Fatal(Error::Http {
                            url: self.display(),
                            message: format!("asked for {range}, server sent bytes from {start}"),
                        }));
                    }
                    // Some servers cap the size of a range.
                    let capped = served
                        .filter(|&(_, last)| last < end)
                        .map(|(_, last)| (last + 1 - offset) as usize);
                    let expect = capped.unwrap_or(want);
                    let data = read_body(resp.into_body().into_reader(), expect as u64)
                        .map_err(io_retry)?;
                    let at_eof = self.size.is_some_and(|s| offset + data.len() as u64 >= s);
                    if data.len() < expect && !at_eof && (self.size.is_some() || capped.is_some()) {
                        return Err(Failure::Retry(
                            Error::Http {
                                url: self.display(),
                                message: format!(
                                    "short range response: {} of {expect} bytes",
                                    data.len()
                                ),
                            },
                            None,
                        ));
                    }
                    Ok((data, capped.is_some()))
                }
                416 => {
                    let _ = read_body(resp.into_body().into_reader(), 64 << 10);
                    Ok((Vec::new(), false))
                }
                200 if offset == 0 => {
                    // Range ignored for this request; the start of the
                    // body is still the data we asked for.
                    read_body(resp.into_body().into_reader(), want as u64)
                        .map(|d| (d, false))
                        .map_err(io_retry)
                }
                s => Err(self.refused(s, after)),
            }
        })
    }

    /// What a range request's status means when it isn't the data asked for.
    fn refused(&self, status: u16, after: Option<Duration>) -> Failure {
        match status {
            200 => Failure::Fatal(Error::Unsupported(format!(
                "{} stopped honouring range requests",
                self.display()
            ))),
            s if is_transient_status(s) => Failure::Retry(
                Error::HttpStatus {
                    status: s,
                    url: self.display(),
                },
                after,
            ),
            s => Failure::Fatal(status_error(s, &self.url).unwrap_or(Error::HttpStatus {
                status: s,
                url: self.display(),
            })),
        }
    }

    /// Starts one request for `len` bytes at `offset`, with no overall
    /// deadline on its body: it can run long, or sit unread while the cache
    /// is full. A server that stops sending still fails it at the read
    /// timeout.
    fn open_long_range(&self, offset: u64, len: u64) -> Result<RangeBody> {
        let size = self
            .size
            .ok_or_else(|| Error::Unsupported(format!("{}: size unknown", self.display())))?;
        let end = (offset + len - 1).min(size.saturating_sub(1));
        let range = format!("bytes={offset}-{end}");
        self.client.retrying(&self.url, || {
            let resp = self.client.send_once(
                "GET",
                &self.url,
                &[("Range", range.as_str())],
                None,
                true,
            )?;
            let status = resp.status().as_u16();
            if status != 206 {
                return Err(self.refused(status, retry_after(&resp)));
            }
            let served = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|c| c.range);
            if let Some((start, _)) = served
                && start != offset
            {
                return Err(Failure::Fatal(Error::Http {
                    url: self.display(),
                    message: format!("asked for {range}, server sent bytes from {start}"),
                }));
            }
            let last = served.map_or(end, |(_, last)| last.min(end));
            Ok(RangeBody {
                reader: Box::new(resp.into_body().into_reader()),
                len: last + 1 - offset,
            })
        })
    }

    fn fetch_streaming(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut guard = lock(&self.stream);
        let max_skip = self.client.opts.max_forward_skip;
        let reopen = match guard.as_ref() {
            Some(s) => offset < s.pos,
            None => true,
        };
        if reopen {
            // Without ranges the only way back is to restart the download
            // from byte 0, which is only reasonable near the start.
            if offset > max_skip {
                return Err(Error::Unsupported(match guard.as_ref() {
                    Some(s) => format!(
                        "{} does not support range requests: cannot seek backward from byte {} to {offset}",
                        self.display(),
                        s.pos
                    ),
                    None => format!(
                        "{} does not support range requests: cannot seek to byte {offset}",
                        self.display()
                    ),
                }));
            }
            log::debug!(
                "{}: no range support, restarting stream to reach byte {offset}",
                self.display()
            );
            let resp = self.client.retrying(&self.url, || {
                let resp = self.client.send_once("GET", &self.url, &[], None, true)?;
                let status = resp.status().as_u16();
                if is_transient_status(status) {
                    return Err(Failure::Retry(
                        Error::HttpStatus {
                            status,
                            url: self.display(),
                        },
                        retry_after(&resp),
                    ));
                }
                if let Some(e) = status_error(status, &self.url) {
                    return Err(Failure::Fatal(e));
                }
                Ok(resp)
            })?;
            *guard = Some(Stream {
                reader: Box::new(resp.into_body().into_reader()),
                pos: 0,
            });
        }
        let Some(stream) = guard.as_mut() else {
            return Err(Error::Unsupported("stream unavailable".into()));
        };
        if offset - stream.pos > max_skip {
            return Err(Error::Unsupported(format!(
                "{} does not support range requests; refusing to skip {} bytes",
                self.display(),
                offset - stream.pos
            )));
        }
        let skip = offset - stream.pos;
        let skipped = io::copy(&mut (&mut stream.reader).take(skip), &mut io::sink());
        let result = skipped.and_then(|n| {
            stream.pos += n;
            if n < skip {
                return Ok(Vec::new());
            }
            let mut out = Vec::with_capacity(len);
            (&mut stream.reader)
                .take(len as u64)
                .read_to_end(&mut out)?;
            stream.pos += out.len() as u64;
            Ok(out)
        });
        result.map_err(|e| {
            // The stream is unusable after an error; a later read reopens it.
            *guard = None;
            Error::Http {
                url: self.display(),
                message: format!("stream read failed: {e}"),
            }
        })
    }
}

impl RangeFetch for HttpFetcher {
    fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let r = if self.ranges {
            self.fetch_range(offset, len)
        } else {
            self.fetch_streaming(offset, len)
        };
        r.map_err(io::Error::from)
    }

    fn size(&self) -> Option<u64> {
        self.size
    }

    fn describe(&self) -> String {
        self.display()
    }

    fn sequential_only(&self) -> bool {
        !self.ranges
    }

    fn long_ranges(&self) -> bool {
        self.ranges && self.size.is_some()
    }

    fn open_range(&self, offset: u64, len: u64) -> io::Result<RangeBody> {
        self.open_long_range(offset, len).map_err(io::Error::from)
    }
}

/// A file read over HTTP(S) with Range requests, a block cache and
/// read-ahead. See the [module documentation](self).
pub struct HttpFile {
    src: CachedSource<HttpFetcher>,
}

impl HttpFile {
    /// Opens `url` with one probe request (`Range: bytes=0-<block>`) that
    /// learns the size, whether ranges work, and fills the first block.
    pub fn open(client: &HttpClient, url: &str) -> Result<HttpFile> {
        // Embedded `user:pass@` stays in the URL: `send_once` strips it into
        // an Authorization header on every request.
        let url = url.to_string();
        let bs = client.opts.cache.block_size.max(4096) as u64;
        let range = format!("bytes=0-{}", bs - 1);
        let display = redact(&url);
        enum Probe {
            Ranged(Option<u64>, Vec<u8>),
            Plain(Option<u64>, Response),
        }
        let probe = client.retrying(&url, || {
            let resp = client.send_once("GET", &url, &[("Range", range.as_str())], None, true)?;
            let status = resp.status().as_u16();
            let cr = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match status {
                206 => {
                    let total = cr.and_then(|c| c.total);
                    let want = total.map_or(bs, |t| t.min(bs));
                    let data = read_body(resp.into_body().into_reader(), want).map_err(|e| {
                        Failure::Retry(
                            Error::Http {
                                url: display.clone(),
                                message: e.to_string(),
                            },
                            None,
                        )
                    })?;
                    Ok(Probe::Ranged(total, data))
                }
                416 => {
                    let total = cr.and_then(|c| c.total).unwrap_or(0);
                    let _ = read_body(resp.into_body().into_reader(), 64 << 10);
                    Ok(Probe::Ranged(Some(total), Vec::new()))
                }
                200 => {
                    let len = resp
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse().ok());
                    Ok(Probe::Plain(len, resp))
                }
                s if is_transient_status(s) => Err(Failure::Retry(
                    Error::HttpStatus {
                        status: s,
                        url: display.clone(),
                    },
                    retry_after(&resp),
                )),
                s => Err(Failure::Fatal(status_error(s, &url).unwrap_or(
                    Error::HttpStatus {
                        status: s,
                        url: display.clone(),
                    },
                ))),
            }
        })?;
        let opts = client.opts.cache.clone();
        let file = match probe {
            Probe::Ranged(size, first) => {
                let src = CachedSource::new(
                    HttpFetcher {
                        client: client.clone(),
                        url,
                        size,
                        ranges: true,
                        stream: Mutex::new(None),
                    },
                    &opts,
                );
                // Only a whole first block (or the whole file): the cache
                // takes a short block for the file's last, and a server that
                // caps its ranges or broke off sent less.
                let whole = size.map_or(bs, |s| s.min(bs));
                if first.len() as u64 == whole {
                    src.seed(0, first);
                }
                HttpFile { src }
            }
            Probe::Plain(size, resp) => {
                log::info!("{display}: server ignores range requests; streaming sequentially");
                let mut reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
                let mut first = Vec::new();
                (&mut reader)
                    .take(bs)
                    .read_to_end(&mut first)
                    .map_err(|e| Error::Http {
                        url: display.clone(),
                        message: e.to_string(),
                    })?;
                // A short first block means we already have the whole file.
                let size = if (first.len() as u64) < bs {
                    Some(first.len() as u64)
                } else {
                    size
                };
                let pos = first.len() as u64;
                let src = CachedSource::new(
                    HttpFetcher {
                        client: client.clone(),
                        url,
                        size,
                        ranges: false,
                        stream: Mutex::new(Some(Stream { reader, pos })),
                    },
                    &opts,
                );
                if !first.is_empty() {
                    src.seed(0, first);
                }
                HttpFile { src }
            }
        };
        Ok(file)
    }

    /// True when the server honours Range requests (random access works).
    pub fn supports_ranges(&self) -> bool {
        self.src.fetcher().ranges
    }

    /// Cache counters for this file.
    pub fn stats(&self) -> CacheStats {
        self.src.stats()
    }

    /// Bytes per cache block.
    pub fn block_size(&self) -> u64 {
        self.src.block_size()
    }
}

impl ByteSource for HttpFile {
    fn size(&self) -> Option<u64> {
        self.src.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.src.read_at(offset, buf)
    }

    fn describe(&self) -> String {
        self.src.describe()
    }
}

impl std::fmt::Debug for HttpFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpFile")
            .field("url", &self.src.describe())
            .field("size", &self.src.size())
            .field("ranges", &self.supports_ranges())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::Instant;

    #[derive(Debug)]
    struct ConnectAttempt {
        uri: http::Uri,
        addresses: Vec<SocketAddr>,
        timeout: Duration,
    }

    #[derive(Debug)]
    struct ScriptedConnector {
        clock: Arc<Mutex<Instant>>,
        steps: Mutex<VecDeque<(Duration, std::result::Result<(), ureq::Error>)>>,
        attempts: Mutex<Vec<ConnectAttempt>>,
    }

    type ConnectResult = std::result::Result<Option<Box<dyn Transport>>, ureq::Error>;

    impl Connector<()> for ScriptedConnector {
        type Out = Box<dyn Transport>;

        fn connect(
            &self,
            details: &ConnectionDetails,
            _: Option<()>,
        ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
            lock(&self.attempts).push(ConnectAttempt {
                uri: details.uri.clone(),
                addresses: details.addrs.iter().copied().collect(),
                timeout: *details.timeout.after,
            });
            let (elapsed, result) = lock(&self.steps).pop_front().expect("planned connect");
            *lock(&self.clock) += elapsed;
            result.map(|()| None)
        }
    }

    fn scripted_address_fallback(
        uri: &str,
        config: &ureq::config::Config,
        addresses: &[SocketAddr],
        steps: Vec<(Duration, std::result::Result<(), ureq::Error>)>,
    ) -> (ConnectResult, Vec<ConnectAttempt>) {
        use ureq::unversioned::transport::time::Instant as ConnectInstant;

        let now = Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let connector = AddressFallbackConnector {
            inner: ScriptedConnector {
                clock: clock.clone(),
                steps: Mutex::new(steps.into()),
                attempts: Mutex::new(Vec::new()),
            },
            fallback: AddressFallback {
                scheme: "https".into(),
                host: "server.plex.direct".into(),
                port: 32400,
                address: "192.168.1.10:32400".parse().unwrap(),
            },
        };
        let resolver = DefaultResolver::default();
        let mut addrs = resolver.empty();
        for address in addresses {
            addrs.push(*address);
        }
        let uri = uri.parse().unwrap();
        let details = ConnectionDetails {
            uri: &uri,
            addrs,
            config,
            request_level: false,
            resolver: &resolver,
            now: ConnectInstant::Exact(now),
            timeout: NextTimeout {
                after: Duration::from_secs(10).into(),
                reason: ureq::Timeout::Connect,
                ..NextTimeout::default()
            },
            current_time: Arc::new(move || ConnectInstant::Exact(*lock(&clock))),
            run_connector: Arc::new(|_| Err(ureq::Error::ConnectionFailed)),
            register_shutdown: Arc::new(|_| Ok(())),
        };
        let result = connector.connect(&details, None);
        let attempts = std::mem::take(&mut *lock(&connector.inner.attempts));
        (result, attempts)
    }

    #[test]
    fn an_advertised_address_gets_the_remaining_connect_budget_after_a_timeout() {
        let dns = "192.0.2.1:32400".parse().unwrap();
        let advertised = "192.168.1.10:32400".parse().unwrap();
        let config = ureq::Agent::config_builder().proxy(None).build();
        let uri = "https://server.plex.direct:32400/identity";
        // This also models a TCP connection that succeeded before its TLS
        // handshake timed out: the wrapper retries the complete connector.
        let (result, attempts) = scripted_address_fallback(
            uri,
            &config,
            &[dns, advertised],
            vec![
                (
                    Duration::from_secs(5),
                    Err(ureq::Error::Timeout(ureq::Timeout::Connect)),
                ),
                (Duration::ZERO, Ok(())),
            ],
        );
        assert!(result.is_ok());
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].addresses, [dns]);
        assert_eq!(attempts[1].addresses, [advertised]);
        assert_eq!(attempts[0].timeout, Duration::from_secs(5));
        assert_eq!(attempts[1].timeout, Duration::from_secs(5));
        assert!(attempts.iter().all(|attempt| attempt.uri == uri));
    }

    #[test]
    fn address_fallback_preserves_failures_and_never_retries_certificate_errors() {
        let dns = "192.0.2.1:32400".parse().unwrap();
        let advertised: SocketAddr = "192.168.1.10:32400".parse().unwrap();
        let config = ureq::Agent::config_builder().proxy(None).build();
        let uri = "https://server.plex.direct:32400/identity";
        let (result, attempts) = scripted_address_fallback(
            uri,
            &config,
            &[dns],
            vec![
                (
                    Duration::from_secs(2),
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused).into()),
                ),
                (
                    Duration::ZERO,
                    Err(ureq::Error::Timeout(ureq::Timeout::Connect)),
                ),
            ],
        );
        assert_eq!(attempts[1].timeout, Duration::from_secs(8));
        let error = result.unwrap_err().to_string();
        assert!(error.contains(&advertised.to_string()), "{error}");
        assert!(error.contains("timeout: connect"), "{error}");

        for error in [
            ureq::Error::Tls("invalid certificate"),
            io::Error::new(io::ErrorKind::InvalidData, "invalid certificate").into(),
            ureq::Error::Timeout(ureq::Timeout::Global),
        ] {
            let (result, attempts) =
                scripted_address_fallback(uri, &config, &[dns], vec![(Duration::ZERO, Err(error))]);
            assert!(result.is_err());
            assert_eq!(attempts.len(), 1);
        }
    }

    #[test]
    fn address_fallback_is_scoped_respects_proxies_and_does_not_repeat_an_address() {
        let advertised: SocketAddr = "192.168.1.10:32400".parse().unwrap();
        let config = ureq::Agent::config_builder().proxy(None).build();
        let (result, attempts) = scripted_address_fallback(
            "https://server.plex.direct:32400/identity",
            &config,
            &[advertised],
            vec![(Duration::ZERO, Err(ureq::Error::ConnectionFailed))],
        );
        assert!(result.is_err());
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].timeout, Duration::from_secs(10));
        for uri in [
            "http://server.plex.direct:32400/identity",
            "https://server.plex.direct:32401/identity",
            "https://other.plex.direct:32400/identity",
        ] {
            let (result, attempts) = scripted_address_fallback(
                uri,
                &config,
                &["192.0.2.1:32400".parse().unwrap()],
                vec![(Duration::ZERO, Err(ureq::Error::ConnectionFailed))],
            );
            assert!(result.is_err());
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].timeout, Duration::from_secs(10));
        }
        let config = ureq::Agent::config_builder()
            .proxy(Some(ureq::Proxy::new("http://127.0.0.1:8888").unwrap()))
            .build();
        let (result, attempts) = scripted_address_fallback(
            "https://server.plex.direct:32400/identity",
            &config,
            &["192.0.2.1:32400".parse().unwrap()],
            vec![(Duration::ZERO, Err(ureq::Error::ConnectionFailed))],
        );
        assert!(result.is_err());
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].timeout, Duration::from_secs(10));
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-1023/4096"),
            Some(ContentRange {
                range: Some((0, 1023)),
                total: Some(4096)
            })
        );
        assert_eq!(
            parse_content_range("bytes 10-19/*"),
            Some(ContentRange {
                range: Some((10, 19)),
                total: None
            })
        );
        assert_eq!(
            parse_content_range("bytes */77"),
            Some(ContentRange {
                range: None,
                total: Some(77)
            })
        );
        assert_eq!(parse_content_range("bytes 5-1/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn statuses() {
        assert!(status_error(204, "u").is_none());
        assert!(matches!(status_error(401, "u"), Some(Error::Auth(_))));
        assert!(matches!(status_error(404, "u"), Some(Error::NotFound(_))));
        assert!(matches!(
            status_error(500, "u"),
            Some(Error::HttpStatus { status: 500, .. })
        ));
        assert!(is_transient_status(503));
        assert!(!is_transient_status(404));
    }

    #[test]
    fn debug_hides_auth() {
        let c = HttpClient::new(
            HttpOptions::default(),
            Some(&Credentials::new("bob", "hunter2")),
        );
        let s = format!("{c:?}");
        assert!(!s.contains("hunter2") && !s.contains("Ym9i"), "{s}");
    }
}
