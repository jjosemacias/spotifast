use super::super::{Allowance, Lease, Terminal, TransportStage, http_reply};
use super::*;
use crate::backend::jam::{DEADLINE, FRESH_FOR};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Body, Bytes, Frame};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use std::{
    fmt,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{oneshot, watch},
};

fn ok<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic fixture operation failed"),
    }
}
struct Unknown;
impl fmt::Debug for Unknown {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("must not format error")
    }
}
impl fmt::Display for Unknown {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("must not format error")
    }
}
impl StdError for Unknown {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        panic!("must not call unknown source")
    }
}

#[test]
fn typed_failure_wrappers_are_bounded_and_never_format_or_traverse_unknown() {
    assert_eq!(classify(&Unknown), FutureFailure::OPAQUE);
    let unknown = std::io::Error::new(std::io::ErrorKind::BrokenPipe, Unknown);
    assert_eq!(
        classify(&unknown),
        FutureFailure {
            hyper: None,
            cause: FutureCause::Io(IoKind::BrokenPipe)
        }
    );
    let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
    assert_eq!(
        classify(&std::io::Error::other(tls)).cause,
        FutureCause::Tls(TlsKind::UnknownIssuer)
    );
    let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForNameContext {
        expected: rustls::pki_types::ServerName::try_from("SyntheticPrivate.invalid").unwrap(),
        presented: vec!["SyntheticPrivate.invalid".to_owned()],
    });
    let projected = classify(&tls);
    assert_eq!(projected.cause, FutureCause::Tls(TlsKind::NameRejected));
    assert_eq!(
        format!("{projected:?}"),
        "FutureFailure { hyper: None, cause: Tls(NameRejected) }"
    );
    // Seven IO wrappers + leaf are eight recognized nodes. An eighth wrapper
    // cuts before the leaf and preserves only the public IO kind.
    for wrappers in [7, 8] {
        let mut error: Box<dyn StdError + Send + Sync> = Box::new(
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        for _ in 0..wrappers {
            error = Box::new(std::io::Error::new(std::io::ErrorKind::BrokenPipe, error));
        }
        assert_eq!(
            classify(&*error).cause,
            if wrappers == 7 {
                FutureCause::Tls(TlsKind::UnknownIssuer)
            } else {
                FutureCause::Io(IoKind::BrokenPipe)
            }
        );
    }
    for code in 0..=13 {
        assert_eq!(H2Reason::from_standard(code), Some(H2Reason(code as u8)));
    }
    assert!(H2Reason::from_standard(14).is_none() && H2Reason::from_standard(u32::MAX).is_none());
    assert_eq!(
        classify(&h2::Error::from(h2::Reason::from(14))).cause,
        FutureCause::Opaque
    );
    assert_eq!(IoKind::from(std::io::ErrorKind::InvalidData), IoKind::Other);
}

struct BodyFailure(bool);
impl Body for BodyFailure {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.0 {
            Poll::Ready(None)
        } else {
            self.0 = true;
            Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                Unknown,
            ))))
        }
    }
}
#[derive(Clone, Copy)]
enum Mode {
    ResetProtocol,
    ResetCancel,
    GoAway,
    H2IoClose,
    Parse,
    Close,
    UserBody,
    DropPending,
}
impl Mode {
    fn h2(self) -> bool {
        matches!(
            self,
            Self::ResetProtocol | Self::ResetCancel | Self::GoAway | Self::H2IoClose
        )
    }
}
async fn peer(socket: TcpStream, mode: Mode, ack: oneshot::Sender<()>) {
    if mode.h2() {
        let mut connection = ok(h2::server::handshake(socket).await);
        let (_request, mut response) =
            ok(connection.accept().await.expect("one synthetic request"));
        let _ = ack.send(());
        match mode {
            Mode::ResetProtocol => response.send_reset(h2::Reason::PROTOCOL_ERROR),
            Mode::ResetCancel => response.send_reset(h2::Reason::CANCEL),
            Mode::GoAway => connection.abrupt_shutdown(h2::Reason::HTTP_1_1_REQUIRED),
            Mode::H2IoClose => {
                drop(response);
                drop(connection);
                return;
            }
            _ => unreachable!(),
        }
        while connection.accept().await.is_some() {}
    } else {
        let mut socket = socket;
        let mut head = Vec::new();
        loop {
            let mut part = [0u8; 1024];
            let n = match socket.read(&mut part).await {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 {
                return;
            }
            head.extend_from_slice(&part[..n]);
            assert!(head.len() < 8192);
            if head.windows(4).any(|x| x == b"\r\n\r\n") {
                break;
            }
        }
        let _ = ack.send(());
        match mode {
            Mode::Parse => {
                let _ = socket.write_all(b"HTTP/1.1 XXX Invalid\r\n\r\n").await;
            }
            Mode::DropPending => {
                let _socket = socket;
                std::future::pending::<()>().await;
                return;
            }
            _ => {}
        }
        let _ = socket.shutdown().await;
    }
}

#[tokio::test]
async fn typed_failure_real_client_sources_reach_the_existing_terminal_mapper() {
    for mode in [
        Mode::ResetProtocol,
        Mode::ResetCancel,
        Mode::GoAway,
        Mode::H2IoClose,
        Mode::Parse,
        Mode::Close,
        Mode::UserBody,
        Mode::DropPending,
    ] {
        let listener = ok(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await);
        let address = ok(listener.local_addr());
        assert!(address.ip().is_loopback());
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, address) = ok(listener.accept().await);
            assert!(address.ip().is_loopback());
            peer(socket, mode, tx).await;
        });
        let client: Client<_, BoxBody<Bytes, std::io::Error>> =
            Client::builder(TokioExecutor::new())
                .http2_only(mode.h2())
                .retry_canceled_requests(false)
                .build_http();
        let body = if matches!(mode, Mode::UserBody) {
            BodyFailure(false).boxed()
        } else {
            Full::new(Bytes::from_static(b"{}"))
                .map_err(|never| -> std::io::Error { match never {} })
                .boxed()
        };
        let request = ok(http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://{address}/fixture"))
            .body(body));
        let mut response = Box::pin(client.request(request));
        if matches!(mode, Mode::DropPending) {
            ok(tokio::time::timeout(Duration::from_secs(3),async {tokio::select! {_=&mut response=>panic!("unexpected response before drop"),ack=rx=>ok(ack)}}).await);
            drop(response);
            drop(client);
            server.abort();
            continue;
        }
        let mut observed = None;
        let result = ok(tokio::time::timeout(
            Duration::from_secs(3),
            http_reply(
                || Ok::<_, ()>(response),
                &|| Ok(()),
                |error| {
                    assert!(!error.is_connect());
                    let projection = project(&error);
                    if matches!(mode, Mode::GoAway) {
                        let hyper = error
                            .source()
                            .and_then(|x| x.downcast_ref::<hyper::Error>());
                        let inner = hyper
                            .and_then(|x| x.source())
                            .and_then(|x| x.downcast_ref::<h2::Error>());
                        if inner.and_then(|x| x.reason()).is_some() {
                            assert_eq!(
                                projection.cause,
                                FutureCause::H2GoAway(H2Reason(13), H2Origin::Remote)
                            );
                        } else {
                            let io = hyper
                                .and_then(|x| x.source())
                                .and_then(|x| x.downcast_ref::<std::io::Error>())
                                .expect("typed IO fallback");
                            assert_eq!(projection.cause, FutureCause::Io(IoKind::from(io.kind())));
                        }
                    }
                    observed = Some(projection);
                    projection
                },
            ),
        )
        .await);
        let projection = observed.expect("native error projected once");
        assert_eq!(
            result.err(),
            Some(Terminal::Transport(TransportStage::AwaitHeaders(
                projection
            )))
        );
        match mode {
            Mode::ResetProtocol => assert_eq!(
                projection.cause,
                FutureCause::H2Reset(H2Reason(1), H2Origin::Remote)
            ),
            Mode::ResetCancel => assert_eq!(
                projection.cause,
                FutureCause::H2Reset(H2Reason(8), H2Origin::Remote)
            ),
            Mode::Parse => assert_eq!(projection.hyper, Some(HyperContext::Parse)),
            Mode::UserBody => assert_eq!(projection.hyper, Some(HyperContext::User)),
            Mode::H2IoClose => assert!(matches!(projection.cause, FutureCause::Io(_))),
            Mode::Close => assert!(
                projection.hyper == Some(HyperContext::Incomplete)
                    || matches!(projection.cause, FutureCause::Io(_))
            ),
            _ => {}
        }
        let text = format!("{projection:?}");
        for private in [
            "http:",
            "127.0.0.1",
            "fixture",
            "UnknownIssuer(",
            "SyntheticPrivate",
        ] {
            assert!(!text.contains(private));
        }
        drop(client);
        server.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn typed_failure_context_retirement_prevents_even_classification() {
    let (epoch, receiver) = watch::channel(0);
    let lease = Lease::new(
        receiver,
        Arc::new(Allowance::default()),
        tokio::time::Instant::now() + FRESH_FOR,
        tokio::time::Instant::now() + DEADLINE,
    );
    let classified = AtomicBool::new(false);
    let result = http_reply(
        || {
            Ok::<_, ()>(async {
                epoch.send_replace(1);
                Err::<http::Response<Full<&[u8]>>, _>(std::io::Error::from(
                    std::io::ErrorKind::ConnectionReset,
                ))
            })
        },
        &|| lease.check(),
        |error| {
            classified.store(true, Ordering::SeqCst);
            classify(&error)
        },
    )
    .await;
    assert_eq!(result.err(), Some(Terminal::Invalidated));
    assert!(!classified.load(Ordering::SeqCst));
}
