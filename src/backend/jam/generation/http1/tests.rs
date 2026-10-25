use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const CERT: &[u8] = include_bytes!("../../../../testdata/jam-http1/cert.der");
const KEY: &[u8] = include_bytes!("../../../../testdata/jam-http1/key.der");

fn tls() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(CERT.to_vec())).unwrap();
    tls_roots(roots)
}

fn tls_roots(roots: rustls::RootCertStore) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn server_tls() -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(CERT.to_vec())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
    )
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

fn prepared() -> http::Request<Vec<u8>> {
    request(
        &JoinUri::capture(&serde_json::json!({"join_session_uri":"hm:synthetic/\"é"})).unwrap(),
        Auth {
            bearer: "Bearer SyntheticNotASecret".into(),
            client_token: "SyntheticClient".into(),
        },
    )
    .unwrap()
}

async fn head(stream: &mut (impl AsyncReadExt + Unpin)) -> Vec<u8> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.unwrap());
        assert!(bytes.len() <= 8192);
    }
    bytes
}

// Real TLS and HTTP1 receptor; CONNECT is handled locally, never forwarded.
async fn wire_case(proxy: bool, status: u16) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let mut request = prepared();
    if !proxy {
        // Direct path must address the literal loopback listener; Host, path,
        // headers and body stay identical. CONNECT case keeps the URI exact too.
        *request.uri_mut() = format!("https://{address}/url-dispenser/v1/generate-url")
            .parse()
            .unwrap();
    }
    let expected_headers = request.headers().clone();
    let expected_body = request.body().clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        if proxy {
            let connect = head(&mut socket).await;
            assert_eq!(connect, b"CONNECT spclient.wg.spotify.com:443 HTTP/1.1\r\nHost: spclient.wg.spotify.com:443\r\n\r\n");
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
        }
        let hello =
            tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), socket)
                .await
                .unwrap();
        let offered: Vec<_> = hello
            .client_hello()
            .alpn()
            .unwrap()
            .map(|p| p.to_vec())
            .collect();
        assert_eq!(offered, vec![b"http/1.1".to_vec()]);
        let mut stream = hello.into_stream(server_tls()).await.unwrap();
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        let bytes = head(&mut stream).await;
        let lines = std::str::from_utf8(&bytes).unwrap();
        let mut lines = lines.split("\r\n");
        assert_eq!(
            lines.next(),
            Some(if proxy {
                // Pinned ProxyStream::Secured reports proxy(true), just as in
                // Session's connector, so Hyper uses absolute-form after CONNECT.
                "POST https://spclient.wg.spotify.com/url-dispenser/v1/generate-url HTTP/1.1"
            } else {
                "POST /url-dispenser/v1/generate-url HTTP/1.1"
            })
        );
        let mut actual = http::HeaderMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(": ").unwrap();
            assert!(
                actual
                    .insert(
                        http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        http::HeaderValue::from_str(value).unwrap()
                    )
                    .is_none()
            );
        }
        let mut expected = expected_headers;
        expected.insert(http::header::USER_AGENT, user_agent());
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 7);
        let mut body = vec![0; expected_body.len()];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(body, expected_body);
        if status != 0 {
            stream.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Length: 0\r\nLocation: https://{address}/forbidden-replay\r\nRetry-After: 0\r\n\r\n").as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        }
        // Any replay on this connection is a failure. A new TCP connection is
        // independently counted below. Drop for the disconnect/error case.
        if status != 0 {
            let mut extra = [0];
            match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut extra))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
                _ => panic!("connection retained or request replayed"),
            }
        }
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let (_epoch, receiver) = watch::channel(0);
    let allowance = Arc::new(Allowance::default());
    let lease = Lease::new(
        receiver,
        Arc::clone(&allowance),
        Instant::now() + FRESH_FOR,
        Instant::now() + DEADLINE,
    );
    let proxy_url = reqwest::Url::parse(&format!("http://{address}")).unwrap();
    lease.dispatch().unwrap();
    let result = send_with(request, &|| lease.check(), || {
        client(tls(), proxy.then_some(&proxy_url))
    })
    .await;
    if status == 0 {
        assert!(matches!(
            result,
            Err(Terminal::Transport(TransportStage::AwaitHeaders(_)))
        ));
    } else {
        let reply = result.unwrap();
        assert_eq!(reply.status, status);
        assert_eq!(output(reply), Err(Terminal::Http(status)));
    }
    assert!(allowance.exhausted());
    assert_eq!(lease.dispatch(), Err(Terminal::Exhausted));
    server.await.unwrap();
}

#[tokio::test]
async fn h1_connector_keeps_certificate_trust_and_name_validation() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for wrong_name in [false, true] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let proxy =
                reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                head(&mut socket).await;
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                assert!(
                    tokio_rustls::TlsAcceptor::from(server_tls())
                        .accept(socket)
                        .await
                        .is_err()
                );
            });
            let mut config = tls();
            let mut request = prepared();
            if wrong_name {
                *request.uri_mut() = "https://uncovered.invalid/url-dispenser/v1/generate-url"
                    .parse()
                    .unwrap();
            } else {
                config = tls_roots(rustls::RootCertStore::empty());
            }
            let result = send_with(request, &|| Ok(()), || client(config, Some(&proxy))).await;
            assert!(matches!(
                result,
                Err(Terminal::Transport(TransportStage::AwaitHeaders(_)))
            ));
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn literal_h1_alpn_exact_request_direct_and_connect_never_replays() {
    tokio::time::timeout(Duration::from_secs(15), async {
        wire_case(false, 401).await;
        wire_case(true, 302).await;
        wire_case(true, 429).await;
        wire_case(true, 0).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn setup_fences_expire_cancel_and_retire_before_emission() {
    for mode in 0..6 {
        // Even a regression in the fence must be structurally confined to
        // loopback, never the destination from the unchanged constructor.
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let mut request = prepared();
        *request.uri_mut() = format!(
            "https://{}/url-dispenser/v1/generate-url",
            listener.local_addr().unwrap()
        )
        .parse()
        .unwrap();
        let (epoch, receiver) = watch::channel(0);
        let allowance = Arc::new(Allowance::default());
        let lease = Lease::new(
            receiver,
            Arc::clone(&allowance),
            Instant::now()
                + if mode == 2 {
                    Duration::from_millis(10)
                } else {
                    FRESH_FOR
                },
            Instant::now()
                + if mode == 3 {
                    Duration::from_millis(10)
                } else {
                    DEADLINE
                },
        );
        lease.dispatch().unwrap();
        let check = || lease.check();
        let result = send_with(request, &check, || {
            match mode {
                0 => lease.cancel(),
                1 => {
                    epoch.send_replace(1);
                }
                4 => {
                    epoch.send_replace(1);
                    return Err(Terminal::Transport(TransportStage::PrepareHttp));
                }
                5 => return Err(Terminal::Transport(TransportStage::PrepareHttp)),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
            client(tls(), None)
        });
        let expected = match mode {
            0 => Terminal::Cancelled,
            3 => Terminal::Deadline,
            5 => Terminal::Transport(TransportStage::PrepareHttp),
            _ => Terminal::Invalidated,
        };
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), result).await.unwrap(), Err(error) if error == expected)
        );
        assert!(allowance.exhausted());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn pending_connect_cancellation_retirement_ttl_and_deadline_cannot_send_post() {
    for mode in 0..4 {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (connected, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(head(&mut stream).await.starts_with(b"CONNECT "));
            connected.send(()).unwrap();
            // Withhold CONNECT response: TLS/POST cannot have started.
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let (epoch, receiver) = watch::channel(0);
        let allowance = Arc::new(Allowance::default());
        let lease = Lease::new(
            receiver,
            Arc::clone(&allowance),
            Instant::now() + FRESH_FOR,
            Instant::now() + DEADLINE,
        );
        lease.dispatch().unwrap();
        let check = || lease.check();
        {
            let operation = send_with(prepared(), &check, || client(tls(), Some(&proxy)));
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => panic!("CONNECT unexpectedly completed: {}", result.is_ok()),
                result = tokio::time::timeout(Duration::from_secs(2), waiting) => result.unwrap().unwrap(),
            }
            let expected = match mode {
                0 => {
                    lease.cancel();
                    Terminal::Cancelled
                }
                1 => {
                    epoch.send_replace(1);
                    Terminal::Invalidated
                }
                _ => {
                    tokio::time::pause();
                    tokio::time::advance(if mode == 2 { FRESH_FOR } else { DEADLINE }).await;
                    tokio::time::resume();
                    if mode == 2 {
                        Terminal::Invalidated
                    } else {
                        Terminal::Deadline
                    }
                }
            };
            assert!(matches!(operation.await, Err(error) if error == expected));
        }
        assert!(allowance.exhausted());
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
