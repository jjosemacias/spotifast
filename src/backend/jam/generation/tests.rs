use super::*;
use http_body_util::{Full, Limited};
use std::sync::atomic::AtomicUsize;

type HeaderFixture = std::future::Ready<Result<http::Response<Full<&'static [u8]>>, &'static str>>;

#[tokio::test]
async fn transport_prepare_refusal_consumes_allowance_without_polling_http() {
    let (_epoch, receiver) = watch::channel(0);
    let allowance = Arc::new(Allowance::default());
    let lease = Lease::new(
        receiver,
        Arc::clone(&allowance),
        Instant::now() + FRESH_FOR,
        Instant::now() + DEADLINE,
    );
    let uri =
        JoinUri::capture(&serde_json::json!({"join_session_uri":"hm:SyntheticPrivate"})).unwrap();
    let prepared = request(
        &uri,
        Auth {
            bearer: "Bearer SyntheticPrivate".into(),
            client_token: "SyntheticPrivate".into(),
        },
    )
    .unwrap();
    assert_eq!(prepared.uri(), DESTINATION);
    let calls = AtomicUsize::new(0);
    lease.dispatch().unwrap();
    let result = http_reply(
        || {
            assert_eq!(prepared.method(), Method::POST);
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<HeaderFixture, _>("SyntheticPrivate preparation error")
        },
        &|| lease.check(),
    )
    .await;
    assert_eq!(
        result.err(),
        Some(Terminal::Transport(TransportStage::PrepareHttp))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(allowance.exhausted());
    assert_eq!(lease.dispatch(), Err(Terminal::Exhausted));
}

#[tokio::test]
async fn transport_header_future_failure_has_no_http_or_raw_error() {
    let future: HeaderFixture = std::future::ready(Err("SyntheticPrivate future error"));
    let result = http_reply(|| Ok::<_, &'static str>(future), &|| Ok(())).await;
    let terminal = result.err();
    assert_eq!(
        terminal,
        Some(Terminal::Transport(TransportStage::AwaitHeaders))
    );
    assert_eq!(format!("{terminal:?}"), "Some(Transport(AwaitHeaders))");
}

#[tokio::test]
async fn transport_frame_failure_preserves_only_obtained_http_and_keeps_limits() {
    for status in [200, 401, 503] {
        let response = http::Response::builder()
            .status(status)
            .body(Limited::new(Full::new(&b"SyntheticPrivate body"[..]), 0))
            .unwrap();
        let result = http_reply(
            || Ok::<_, &'static str>(std::future::ready(Ok::<_, &'static str>(response))),
            &|| Ok(()),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(Terminal::Transport(TransportStage::ReadBody(status)))
        );
    }
    let response = http::Response::builder()
        .status(401)
        .body(Full::new(&b"SyntheticPrivate"[..]))
        .unwrap();
    let reply = http_reply(
        || Ok::<_, &'static str>(std::future::ready(Ok::<_, &'static str>(response))),
        &|| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(output(reply), Err(Terminal::Http(401)));
    let body = vec![0; MAX_BODY + 1];
    let response = http::Response::builder()
        .status(200)
        .body(Full::new(body.as_slice()))
        .unwrap();
    assert_eq!(
        http_reply(
            || Ok::<_, &'static str>(std::future::ready(Ok::<_, &'static str>(response))),
            &|| Ok(())
        )
        .await
        .err(),
        Some(Terminal::SchemaRejected)
    );
}

#[tokio::test(start_paused = true)]
async fn transport_fences_withdraw_late_headers_and_body_http() {
    for cancel in [false, true] {
        let (epoch, receiver) = watch::channel(0);
        let lease = Lease::new(
            receiver,
            Arc::new(Allowance::default()),
            Instant::now() + FRESH_FOR,
            Instant::now() + DEADLINE,
        );
        let body = Limited::new(Full::new(&b"SyntheticPrivate"[..]), 0).map_err(|error| {
            if cancel {
                lease.cancel();
            } else {
                epoch.send_replace(1);
            }
            error
        });
        let response = http::Response::builder().status(403).body(body).unwrap();
        let result = http_reply(
            || Ok::<_, &'static str>(std::future::ready(Ok::<_, &'static str>(response))),
            &|| lease.check(),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(if cancel {
                Terminal::Cancelled
            } else {
                Terminal::Invalidated
            })
        );
    }
    let (_epoch, receiver) = watch::channel(0);
    let lease = Lease::new(
        receiver,
        Arc::new(Allowance::default()),
        Instant::now() + FRESH_FOR,
        Instant::now() + DEADLINE,
    );
    let future = async {
        tokio::time::advance(DEADLINE).await;
        Err::<http::Response<Full<&[u8]>>, _>("SyntheticPrivate late future")
    };
    assert_eq!(
        http_reply(|| Ok::<_, &'static str>(future), &|| lease.check())
            .await
            .err(),
        Some(Terminal::Deadline)
    );
}

struct SyntheticLogger {
    trace: Option<&'static str>,
}
impl log::Log for SyntheticLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.trace.is_some_and(|target| target == metadata.target())
    }
    fn log(&self, _: &log::Record<'_>) {
        panic!("preflight must not send any record to sinks");
    }
    fn flush(&self) {}
}

#[tokio::test]
async fn trace_preflight_blocks_authority_before_any_poll_and_after_resume() {
    for target in ["librespot_core::login5", "librespot_core::spclient"] {
        let logger = SyntheticLogger {
            trace: Some(target),
        };
        assert!(!auth_logging_safe(&logger, log::LevelFilter::Trace));
        let acquired = AtomicUsize::new(0);
        let check = || {
            if auth_logging_safe(&logger, log::LevelFilter::Trace) {
                Ok(())
            } else {
                Err(Terminal::PrivacyBlocked)
            }
        };
        let result = fenced(
            async {
                acquired.fetch_add(1, Ordering::SeqCst);
                "SyntheticNotASecret"
            },
            &check,
        )
        .await;
        assert!(matches!(result, Err(Terminal::PrivacyBlocked)));
        assert_eq!(acquired.load(Ordering::SeqCst), 0);
    }
    assert!(auth_logging_safe(
        &SyntheticLogger {
            trace: Some("unrelated")
        },
        log::LevelFilter::Trace
    ));
    assert!(auth_logging_safe(
        &SyntheticLogger {
            trace: Some("librespot_core::login5")
        },
        log::LevelFilter::Debug
    ));
    let checks = AtomicUsize::new(0);
    let acquired = AtomicUsize::new(0);
    let check = || {
        if checks.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(Terminal::PrivacyBlocked)
        }
    };
    let result = fenced(
        async {
            tokio::task::yield_now().await;
            acquired.fetch_add(1, Ordering::SeqCst);
        },
        &check,
    )
    .await;
    assert!(matches!(result, Err(Terminal::PrivacyBlocked)));
    assert_eq!(acquired.load(Ordering::SeqCst), 0);
}

#[test]
fn candidate_exact_request_and_private_output_contract() {
    let uri = JoinUri::capture(
        &serde_json::json!({"join_session_uri":"spotify://sessions/join/SyntheticJam08B"}),
    )
    .unwrap();
    let prepared = request(
        &uri,
        Auth {
            bearer: "Bearer SyntheticLogin5NotASecret08B".into(),
            client_token: "SyntheticClientTokenNotASecret08B".into(),
        },
    )
    .unwrap();
    assert!(prepared.method() == Method::POST && prepared.uri() == DESTINATION);
    assert!(prepared.body() == br#"{"spotify_uri":"spotify://sessions/join/SyntheticJam08B"}"#);
    assert!(prepared.headers().len() == 6 && prepared.headers()["content-length"] == "57");
    for (header, value) in [
        ("host", "spclient.wg.spotify.com"),
        ("authorization", "Bearer SyntheticLogin5NotASecret08B"),
        ("client-token", "SyntheticClientTokenNotASecret08B"),
        ("accept", "application/json"),
        ("content-type", "application/json"),
    ] {
        assert!(prepared.headers()[header] == value);
    }
    assert!(
        prepared.headers()["authorization"].is_sensitive()
            && prepared.headers()["client-token"].is_sensitive()
    );
    for (field, value) in [
        ("join_session_url", "hm:synthetic"),
        ("join_session_token", "synthetic"),
        ("joinSessionUri", "hm:synthetic"),
    ] {
        assert!(JoinUri::capture(&serde_json::json!({field:value})).is_none());
    }
    for value in ["spotify:synthetic", "hm:synthetic"] {
        let uri = JoinUri::capture(&serde_json::json!({"join_session_uri":value})).unwrap();
        let body = request(
            &uri,
            Auth {
                bearer: "Bearer Synthetic".into(),
                client_token: "Synthetic".into(),
            },
        )
        .unwrap()
        .into_body();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["spotify_uri"] == value);
    }
    for (status, mime, body, expected) in [
        (
            200,
            "application/json",
            r#"{"shareable_url":"https://spotify.link/Synthetic"}"#,
            Ok(Family::Short),
        ),
        (
            201,
            "application/json; charset=utf-8",
            r#"{"shareable_url":"https://open.spotify.com/socialsession/Synthetic"}"#,
            Ok(Family::Long),
        ),
        (
            401,
            "text/plain",
            "Synthetic private error",
            Err(Terminal::Http(401)),
        ),
        (200, "text/html", "{}", Err(Terminal::SchemaRejected)),
        (
            200,
            "application/json",
            r#"{"shareable_url":"a","shareable_url":"b"}"#,
            Err(Terminal::SchemaRejected),
        ),
        (
            200,
            "application/json",
            r#"{"shareableUrl":"a"}"#,
            Err(Terminal::SchemaRejected),
        ),
        (
            200,
            "application/json",
            r#"{"shareable_url":"https://evil.invalid/a"}"#,
            Err(Terminal::OutputNotCovered),
        ),
        (
            200,
            "application/json",
            r#"{"shareable_url":"https://spotify.link/a?q=1"}"#,
            Err(Terminal::OutputNotCovered),
        ),
    ] {
        assert_eq!(
            output(Reply {
                status,
                content_type: Some(mime.as_bytes().to_vec()),
                body: body.as_bytes().to_vec()
            }),
            expected
        );
    }
    assert!(Link::parse("https://spotify.link/Synthetic").is_err());
}

#[tokio::test(start_paused = true)]
async fn allowance_survives_epoch_and_deadline_and_never_replays() {
    let (epoch, receiver) = watch::channel(0);
    let allowance = Arc::new(Allowance::default());
    let lease = Lease::new(
        receiver,
        Arc::clone(&allowance),
        Instant::now() + FRESH_FOR,
        Instant::now() + DEADLINE,
    );
    assert_eq!(lease.dispatch(), Ok(()));
    assert_eq!(lease.dispatch(), Err(Terminal::Exhausted));
    epoch.send_replace(1);
    assert_eq!(lease.check(), Err(Terminal::Invalidated));
    let lease = Lease::new(
        epoch.subscribe(),
        allowance,
        Instant::now() + FRESH_FOR,
        Instant::now() + DEADLINE,
    );
    assert_eq!(lease.dispatch(), Err(Terminal::Exhausted));
    tokio::time::advance(DEADLINE).await;
    assert_eq!(lease.check(), Err(Terminal::Deadline));
}
