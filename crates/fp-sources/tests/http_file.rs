//! HTTP ByteSource against a local server: ranges, block cache,
//! read-ahead, Basic auth, retries and the no-range fallback.

mod common;

use base64::Engine;
use common::{Req, TestServer, pattern, resp, serve_bytes};
use fp_core::ByteSource;
use fp_sources::{CacheOptions, Credentials, Error, HttpClient, HttpOptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const AUTH: &str = "Basic Ym9iOnMzY3JldA=="; // bob:s3cret

fn fallback_options() -> HttpOptions {
    HttpOptions {
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
        max_retries: 0,
        ..Default::default()
    }
}

#[test]
fn dns_fallback_keeps_the_original_host_credentials_redirects_and_ranges() {
    let data = Arc::new(pattern(128 * 1024));
    let bytes = data.clone();
    let server = TestServer::start(move |req| {
        if req.path() == "/redirect" {
            return resp(307, "", &[("Location", "/media")]);
        }
        serve_bytes(req, &bytes, true)
    });
    let address: std::net::SocketAddr = server
        .base
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let origin = format!("http://fallback.invalid:{}", address.port());
    let client = HttpClient::new_with_headers(
        HttpOptions {
            cache: CacheOptions {
                block_size: 16 * 1024,
                read_ahead: false,
                ..Default::default()
            },
            ..fallback_options()
        },
        vec![("X-Plex-Token".into(), "private-token".into())],
        &origin,
    )
    .with_address_fallback(address);
    let file = client.open(&format!("{origin}/redirect")).unwrap();
    let mut buf = [0; 1024];
    let n = file.read_at(64 * 1024, &mut buf).unwrap();
    assert_eq!(&buf[..n], &data[64 * 1024..64 * 1024 + n]);
    assert!(file.describe().contains("fallback.invalid"));
    let host = format!("fallback.invalid:{}", address.port());
    let requests = server.requests();
    assert!(requests.len() >= 4, "probe, redirect and a later range");
    assert!(
        requests
            .iter()
            .all(|r| r.header("host") == Some(host.as_str()))
    );
    assert!(
        requests
            .iter()
            .all(|r| r.header("x-plex-token") == Some("private-token"))
    );

    let before = requests.len();
    assert!(
        client
            .get_bytes(
                &format!("http://other.invalid:{}/media", address.port()),
                1024
            )
            .is_err()
    );
    let other_port = if address.port() == u16::MAX {
        address.port() - 1
    } else {
        address.port() + 1
    };
    assert!(
        client
            .get_bytes(&format!("http://fallback.invalid:{other_port}/media"), 1024)
            .is_err()
    );
    assert_eq!(
        server.requests().len(),
        before,
        "fallback is restricted to the original host and port"
    );

    // A direct request to another origin uses ordinary DNS and receives
    // none of the scoped credentials.
    client
        .get_bytes(&server.url("/public"), 256 * 1024)
        .unwrap();
    assert!(
        server
            .requests()
            .last()
            .unwrap()
            .header("x-plex-token")
            .is_none()
    );
}

#[test]
fn address_fallback_recovers_failed_connections_and_keeps_a_working_destination() {
    let destination = TestServer::start(|_| resp(200, "normal DNS", &[]));
    let fallback = TestServer::start(|_| resp(200, "advertised address", &[]));
    let address = fallback
        .base
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let client = HttpClient::new(fallback_options(), None)
        .scoped_to(&destination.base)
        .with_address_fallback(address);
    assert_eq!(
        client.get_bytes(&destination.url("/"), 1024).unwrap(),
        b"normal DNS"
    );
    assert!(fallback.requests().is_empty());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    let origin = format!("http://{closed}");
    let client = HttpClient::new(fallback_options(), None)
        .scoped_to(&origin)
        .with_address_fallback(address);
    assert_eq!(
        client.get_bytes(&origin, 1024).unwrap(),
        b"advertised address",
        "TCP failure after successful DNS must try the advertised socket"
    );
    assert_eq!(fallback.requests().len(), 1);

    let unknown = format!("http://unscoped.invalid:{}/", address.port());
    let client = HttpClient::new(fallback_options(), None).with_address_fallback(address);
    assert!(client.get_bytes(&unknown, 1024).is_err());
    let client = HttpClient::new(fallback_options(), None)
        .scoped_to("not a URL")
        .with_address_fallback(address);
    assert!(client.get_bytes(&unknown, 1024).is_err());
    assert!(
        fallback.requests().len() == 1,
        "invalid or absent scope leaves DNS unchanged"
    );
}

#[test]
fn connection_fallback_preserves_credentials_redirects_and_later_ranges() {
    let data = Arc::new(pattern(128 * 1024));
    let bytes = data.clone();
    let server = TestServer::start(move |request| {
        if request.path() == "/redirect" {
            return resp(307, "", &[("Location", "/media")]);
        }
        serve_bytes(request, &bytes, true)
    });
    let address = server
        .base
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    // localhost resolves normally, but its socket refuses the connection.
    let origin = format!("http://localhost:{}", closed.port());
    let client = HttpClient::new_with_headers(
        HttpOptions {
            cache: CacheOptions {
                block_size: 16 * 1024,
                read_ahead: false,
                ..Default::default()
            },
            ..fallback_options()
        },
        vec![("X-Plex-Token".into(), "private-token".into())],
        &origin,
    )
    .with_address_fallback(address);
    let file = client.open(&format!("{origin}/redirect")).unwrap();
    let mut buf = [0; 1024];
    assert_eq!(file.read_at(64 * 1024, &mut buf).unwrap(), buf.len());
    assert_eq!(buf, data[64 * 1024..64 * 1024 + buf.len()]);
    assert!(file.describe().starts_with(&origin));
    let requests = server.requests();
    assert!(requests.len() >= 4, "redirect, probe and a later range");
    let host = format!("localhost:{}", closed.port());
    for request in &requests {
        assert_eq!(request.header("host"), Some(host.as_str()));
        assert_eq!(request.header("x-plex-token"), Some("private-token"));
    }
}

#[test]
fn address_fallback_keeps_tls_sni_and_verification_after_dns_failure_or_tls_timeout() {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use std::sync::mpsc;

    // Self-signed test identity for a reserved .invalid name. Embedding
    // the DER keeps this regression independent of openssl or a live CA.
    const CERT: &str = "MIIBtjCCAVygAwIBAgIUE/jGo1oiw5MCnX1X/T1vklAQPcgwCgYIKoZIzj0EAwIwHzEdMBsGA1UEAwwUdGxzLWZhbGxiYWNrLmludmFsaWQwIBcNMjYxMDMwMDE1NDE2WhgPMjEyNjEwMDYwMTU0MTZaMB8xHTAbBgNVBAMMFHRscy1mYWxsYmFjay5pbnZhbGlkMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEJ0E8IJZ1K9nTyc1e3FNORoIhlGeH7dHPmDIDcq5cygAWCpwH3kQvyQYbemeK8OkX/U+1UqzlV3mEA4QpefAev6N0MHIwHQYDVR0OBBYEFDO6368fnJpxrIiwRZPlNgxsmaeYMB8GA1UdIwQYMBaAFDO6368fnJpxrIiwRZPlNgxsmaeYMA8GA1UdEwEB/wQFMAMBAf8wHwYDVR0RBBgwFoIUdGxzLWZhbGxiYWNrLmludmFsaWQwCgYIKoZIzj0EAwIDSAAwRQIgaBs/P4a9Vx/ngTLpaddQjnYnhWGKTDwi5XQaXyOw+tICIQCc7tIKu4EatHFMSXnD/KSAvQMQjpTm6mMoltOkCknqvg==";
    const KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtA0h/ljJPG5dxd1BL2rKk+fcYoP0JPNGHzskOlRGWMehRANCAAQnQTwglnUr2dPJzV7cU05GgiGUZ4ft0c+YMgNyrlzKABYKnAfeRC/JBht6Z4rw6Rf9T7VSrOVXeYQDhCl58B6/";
    let cert = CertificateDer::from(
        base64::engine::general_purpose::STANDARD
            .decode(CERT)
            .unwrap(),
    );
    let key = PrivatePkcs8KeyDer::from(
        base64::engine::general_purpose::STANDARD
            .decode(KEY)
            .unwrap(),
    );
    let config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key.into())
            .unwrap(),
    );
    for stall_primary in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = config.clone();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut tls = rustls::ServerConnection::new(config).unwrap();
            let result = tls.complete_io(&mut socket);
            tx.send((tls.server_name().map(str::to_string), result.is_err()))
                .unwrap();
        });
        let mut stalled = None;
        let mut stop = None;
        let origin = if stall_primary {
            let primary = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = primary.local_addr().unwrap().port();
            let (tx, rx) = mpsc::channel();
            stop = Some(tx);
            stalled = Some(std::thread::spawn(move || {
                let (_socket, _) = primary.accept().unwrap();
                // Accept TCP but never answer the TLS handshake.
                rx.recv_timeout(Duration::from_secs(3)).ok();
            }));
            format!("https://localhost:{port}")
        } else {
            format!("https://tls-fallback.invalid:{}", address.port())
        };
        let client = HttpClient::new(
            HttpOptions {
                connect_timeout: Duration::from_millis(600),
                ..fallback_options()
            },
            None,
        )
        .scoped_to(&origin)
        .with_address_fallback(address);
        let started = Instant::now();
        let error = client
            .get_bytes(&format!("{origin}/private"), 1024)
            .unwrap_err();
        if let Some(stop) = stop {
            stop.send(()).ok();
        }
        if let Some(stalled) = stalled {
            stalled.join().unwrap();
        }
        assert!(
            error.to_string().contains("certificate"),
            "expected certificate validation at the fallback after DNS/TLS failure: {error}"
        );
        if stall_primary {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "fallback stays bounded"
            );
        }
        let (name, rejected) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let expected_name = if stall_primary {
            "localhost"
        } else {
            "tls-fallback.invalid"
        };
        assert_eq!(name.as_deref(), Some(expected_name));
        assert!(rejected, "the client must reject this self-signed server");
        assert!(!client.options().insecure_tls);
        server.join().unwrap();
    }
}

#[test]
fn named_credentials_survive_ranges_and_same_origin_redirects_only() {
    let data = Arc::new(pattern(128 * 1024));
    let bytes = data.clone();
    let server = TestServer::start(move |req| {
        if req.header("x-plex-token") != Some("private-token") {
            return resp(401, "unauthorized", &[]);
        }
        if req.path() == "/redirect" {
            return resp(307, "", &[("Location", "/media")]);
        }
        serve_bytes(req, &bytes, true)
    });
    let client = HttpClient::new_with_headers(
        HttpOptions {
            cache: CacheOptions {
                block_size: 16 * 1024,
                read_ahead: false,
                ..Default::default()
            },
            ..Default::default()
        },
        vec![("X-Plex-Token".into(), "private-token".into())],
        &server.base,
    );
    assert!(!format!("{client:?}").contains("private-token"));
    let file = client.open(&server.url("/redirect")).unwrap();
    let mut buf = [0; 1024];
    let n = file.read_at(64 * 1024, &mut buf).unwrap();
    assert_eq!(&buf[..n], &data[64 * 1024..64 * 1024 + n]);
    assert!(server.requests().len() >= 4);
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.header("x-plex-token") == Some("private-token"))
    );

    let target = TestServer::start(|_| resp(200, "unexpected", &[]));
    let destination = target.url("/private");
    let redirect = TestServer::start(move |_| resp(307, "", &[("Location", &destination)]));
    let client = HttpClient::new_with_headers(
        HttpOptions::default(),
        vec![("X-Plex-Token".into(), "private-token".into())],
        &redirect.base,
    );
    assert!(client.get_bytes(&redirect.url("/start"), 1024).is_err());
    assert!(
        target.requests().is_empty(),
        "no token or body reaches another origin"
    );
    client.get_bytes(&target.url("/direct"), 1024).unwrap();
    assert!(
        target
            .requests()
            .iter()
            .all(|r| r.header("x-plex-token").is_none())
    );
}

fn is_get(r: &Req) -> bool {
    r.method == "GET"
}

fn read_all(f: &dyn ByteSource, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = f.read_at(out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn range_requests_cache_and_read_ahead() {
    let data = Arc::new(pattern(3 * (1 << 20) + 512 * 1024));
    let d = data.clone();
    let server = TestServer::start(move |req| {
        if req.header("authorization") != Some(AUTH) {
            return resp(401, "no", &[("WWW-Authenticate", "Basic realm=\"x\"")]);
        }
        serve_bytes(req, &d, true)
    });
    let client = HttpClient::new(
        HttpOptions::default(),
        Some(&Credentials::new("bob", "s3cret")),
    );
    let url = server.url("/v/My%20Clip.mp4");
    let f = client.open(&url).unwrap();
    assert!(f.supports_ranges());
    assert_eq!(f.size(), Some(data.len() as u64));
    assert_eq!(f.block_size(), 1 << 20);
    assert!(f.describe().contains("My%20Clip.mp4"));

    // Demuxer-style sequential reads of 64 KiB through the first 1.5 MiB.
    let mut buf = vec![0u8; 64 * 1024];
    let mut pos = 0usize;
    while pos < 1536 * 1024 {
        let n = f.read_at(pos as u64, &mut buf).unwrap();
        assert_eq!(buf[..n], data[pos..pos + n]);
        pos += n;
    }
    // The probe filled block 0; blocks 1 and 2 came by read-ahead because
    // access was sequential.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.count(is_get) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.count(is_get), 3);
    let st = f.stats();
    assert_eq!((st.fetches, st.read_aheads), (0, 2), "{st:?}");
    let gets_before = server.count(is_get);
    let n = f.read_at(2 << 20, &mut buf).unwrap();
    assert_eq!(buf[..n], data[2 << 20..(2 << 20) + n]);
    // Wait for any read-ahead of block 3 to land, then check block 2 was not refetched.
    std::thread::sleep(Duration::from_millis(100));
    assert!(server.count(is_get) <= gets_before + 1);

    // Seek to the end (moov atom), then back to the start: served from cache.
    let end = data.len() as u64;
    let mut tail = [0u8; 100];
    assert_eq!(f.read_at(end - 100, &mut tail).unwrap(), 100);
    assert_eq!(tail[..], data[data.len() - 100..]);
    assert_eq!(f.read_at(end, &mut tail).unwrap(), 0);
    let gets = server.count(is_get);
    f.read_at(0, &mut buf).unwrap();
    f.read_at(end - 100, &mut tail).unwrap();
    assert_eq!(
        server.count(is_get),
        gets,
        "cached blocks are not refetched"
    );

    // Every request was a bounded range on one of at most 4 blocks.
    for r in server.requests() {
        let range = r.header("range").expect("all GETs are ranged");
        assert!(range.starts_with("bytes="), "{range}");
    }
    assert!(server.count(is_get) <= 5, "{} GETs", server.count(is_get));

    // Whole file in one pass with odd-sized reads.
    assert_eq!(read_all(&f, 200_000), *data);
}

#[test]
fn wrong_credentials_and_missing_files() {
    let server = TestServer::start(|req| {
        if req.path() == "/missing.mp4" {
            return resp(404, "nope", &[]);
        }
        if req.header("authorization") != Some(AUTH) {
            return resp(401, "no", &[]);
        }
        serve_bytes(req, b"0123456789", true)
    });
    let bad = HttpClient::new(
        HttpOptions::default(),
        Some(&Credentials::new("bob", "wrong")),
    );
    let err = bad.open(&server.url("/a.mp4")).unwrap_err();
    assert!(matches!(err, Error::Auth(_)), "{err:?}");
    let err = bad.open(&server.url("/missing.mp4")).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    // 4xx are not retried.
    assert_eq!(server.requests().len(), 2);

    // Credentials embedded in the URL work and never show up in descriptions.
    let anon = HttpClient::new(HttpOptions::default(), None);
    let url = server
        .url("/a.mp4")
        .replace("http://", "http://bob:s3cret@");
    let f = anon.open(&url).unwrap();
    assert!(!f.describe().contains("s3cret"));
    let mut b = [0u8; 4];
    assert_eq!(f.read_at(3, &mut b).unwrap(), 4);
    assert_eq!(&b, b"3456");
    // Small files are a single short block.
    assert_eq!(f.size(), Some(10));
}

#[test]
fn transient_failures_are_retried() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let server = TestServer::start(move |req| {
        if h.fetch_add(1, Ordering::SeqCst) < 2 {
            return resp(503, "busy", &[]);
        }
        serve_bytes(req, b"hello world", true)
    });
    let opts = HttpOptions {
        retry_delay: Duration::from_millis(5),
        ..HttpOptions::default()
    };
    let client = HttpClient::new(opts.clone(), None);
    let f = client.open(&server.url("/x.mp4")).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let mut b = [0u8; 5];
    f.read_at(6, &mut b).unwrap();
    assert_eq!(&b, b"world");

    // Persistent failure gives up after max_retries.
    let always = TestServer::start(|_| resp(503, "down", &[]));
    let client = HttpClient::new(
        HttpOptions {
            max_retries: 2,
            ..opts
        },
        None,
    );
    let err = client.open(&always.url("/x.mp4")).unwrap_err();
    assert!(
        matches!(err, Error::HttpStatus { status: 503, .. }),
        "{err:?}"
    );
    assert_eq!(always.requests().len(), 3);
}

#[test]
fn connection_refused_is_an_error() {
    // Bind and drop a listener to get a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = HttpClient::new(
        HttpOptions {
            max_retries: 1,
            retry_delay: Duration::from_millis(1),
            ..HttpOptions::default()
        },
        None,
    );
    let err = client
        .open(&format!("http://127.0.0.1:{port}/x.mp4"))
        .unwrap_err();
    assert!(matches!(err, Error::Http { .. }), "{err:?}");
}

#[test]
fn servers_without_ranges_stream_sequentially() {
    let data = Arc::new(pattern(1 << 20));
    let d = data.clone();
    let server = TestServer::start(move |req| serve_bytes(req, &d, false));
    let opts = HttpOptions {
        cache: CacheOptions {
            block_size: 64 * 1024,
            max_blocks: 3,
            read_ahead: true,
        },
        max_forward_skip: 128 * 1024,
        ..HttpOptions::default()
    };
    let client = HttpClient::new(opts, None);
    let f = client.open(&server.url("/plain.mp4")).unwrap();
    assert!(!f.supports_ranges());
    assert_eq!(f.size(), Some(data.len() as u64));

    // Sequential reading works through one streamed response.
    assert_eq!(read_all(&f, 48 * 1024), *data);
    assert_eq!(server.count(is_get), 1);

    // A backward seek far from the start, outside the cache, is refused.
    let mut buf = [0u8; 16];
    let err = f.read_at(512 * 1024, &mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
    assert!(err.to_string().contains("range"), "{err}");

    // Near the start it restarts the stream and skips forward.
    assert_eq!(f.read_at(70_000, &mut buf).unwrap(), 16);
    assert_eq!(buf[..], data[70_000..70_016]);
    assert_eq!(server.count(is_get), 2);

    // The cached tail is still served.
    assert_eq!(f.read_at(data.len() as u64 - 16, &mut buf).unwrap(), 16);
    assert_eq!(buf[..], data[data.len() - 16..]);
}

/// Runs `f` on a thread of its own and fails the test if it is still
/// running after `limit`, so a regression can't hang the suite.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).expect("still blocked")
}

/// A server that answers every request with `head` and `body`, sent
/// `pause` apart in `chunks` pieces, and then holds the connection open
/// without sending anything more.
fn raw_server(head: String, body: Vec<u8>, chunks: usize, pause: Duration) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x.mp4", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (head, body) = (head.clone(), body.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let mut stream = &stream;
                let _ = stream.write_all(head.as_bytes());
                for piece in body.chunks(body.len().div_ceil(chunks).max(1)) {
                    std::thread::sleep(pause);
                    let _ = stream.write_all(piece);
                }
                std::thread::sleep(Duration::from_secs(30));
            });
        }
    });
    url
}

fn stall_client() -> HttpClient {
    HttpClient::new(
        HttpOptions {
            read_timeout: Duration::from_millis(300),
            max_retries: 0,
            cache: CacheOptions {
                block_size: 4096,
                read_ahead: false,
                ..CacheOptions::default()
            },
            ..HttpOptions::default()
        },
        None,
    )
}

#[test]
fn a_server_that_stops_after_its_headers_fails_the_open() {
    let ranged = raw_server(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4095/1000000\r\nContent-Length: 4096\r\n\r\n".into(),
        vec![1],
        1,
        Duration::ZERO,
    );
    let plain = raw_server(
        "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n".into(),
        vec![1],
        1,
        Duration::ZERO,
    );
    for url in [ranged, plain] {
        let t = Instant::now();
        let r = within(Duration::from_secs(10), move || {
            stall_client().open(&url).map(|_| ())
        });
        assert!(r.is_err(), "opened a stalled stream");
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "took {:?}",
            t.elapsed()
        );
    }
}

#[test]
fn a_stream_that_stops_later_fails_the_read() {
    let url = raw_server(
        "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n".into(),
        pattern(8192),
        1,
        Duration::ZERO,
    );
    let f = stall_client().open(&url).unwrap();
    let t = Instant::now();
    let r = within(Duration::from_secs(10), move || {
        let mut buf = [0u8; 100];
        f.read_at(9000, &mut buf)
    });
    assert!(r.is_err(), "read past what the server sent");
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "took {:?}",
        t.elapsed()
    );
}

#[test]
fn a_slow_stream_that_keeps_coming_plays_on() {
    // A second in all, longer than the read timeout, but never 300 ms
    // without data.
    let data = pattern(40_000);
    let url = raw_server(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", data.len()),
        data.clone(),
        10,
        Duration::from_millis(100),
    );
    let got = within(Duration::from_secs(10), move || {
        let f = stall_client().open(&url).unwrap();
        read_all(&f, 4096)
    });
    assert!(got == data);
}

/// `serve_bytes` from a server that never sends more than `cap` bytes of a
/// range, saying so in Content-Range as such servers do.
fn capped(req: &Req, data: &[u8], cap: u64) -> common::Resp {
    let mut req = req.clone();
    for (k, v) in req.headers.iter_mut() {
        if k.eq_ignore_ascii_case("range")
            && let Some((a, b)) = v.trim_start_matches("bytes=").split_once('-')
        {
            let start: u64 = a.parse().unwrap();
            let end = b.parse::<u64>().unwrap_or(u64::MAX).min(start + cap - 1);
            *v = format!("bytes={start}-{end}");
        }
    }
    serve_bytes(&req, data, true)
}

#[test]
fn servers_that_cap_their_ranges_are_read_in_full() {
    let data = Arc::new(pattern(20_000));
    let d = data.clone();
    let server = TestServer::start(move |req| capped(req, &d, 1024));
    let client = HttpClient::new(
        HttpOptions {
            cache: CacheOptions {
                block_size: 4096,
                ..CacheOptions::default()
            },
            ..HttpOptions::default()
        },
        None,
    );
    let f = client.open(&server.url("/x.mp4")).unwrap();
    assert_eq!(f.size(), Some(20_000));
    assert!(read_all(&f, 3000) == *data);
}

#[test]
fn a_short_first_range_is_no_end_of_file() {
    // The opening request gets only the first 1024 bytes of its block.
    let data = Arc::new(pattern(8192));
    let d = data.clone();
    let first = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(move |req| {
        if first.fetch_add(1, Ordering::SeqCst) == 0 {
            return resp(
                206,
                d[..1024].to_vec(),
                &[("Content-Range", "bytes 0-1023/8192")],
            );
        }
        serve_bytes(req, &d, true)
    });
    let client = HttpClient::new(
        HttpOptions {
            cache: CacheOptions {
                block_size: 4096,
                ..CacheOptions::default()
            },
            ..HttpOptions::default()
        },
        None,
    );
    let f = client.open(&server.url("/x.mp4")).unwrap();
    let mut buf = [0u8; 100];
    assert_eq!(
        f.read_at(1024, &mut buf).unwrap(),
        100,
        "end of file at 1024"
    );
    assert!(read_all(&f, 3000) == *data);
}
