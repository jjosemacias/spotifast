use std::collections::VecDeque;

use super::*;

fn context() -> Context {
    Context {
        account: "synthetic-account".into(),
        device: "synthetic device&?".into(),
        connection: "synthetic-connection".into(),
        generation: 7,
    }
}

fn session(id: &str, owner: bool) -> Reply {
    Reply {
        status: 200,
        content_type: None,
        body: serde_json::to_vec(&serde_json::json!({
            "session_id": id, "is_session_owner": owner,
            "is_listening": true, "is_controlling": false,
        }))
        .unwrap(),
    }
}

fn status(status: u16) -> Reply {
    Reply {
        status,
        content_type: None,
        body: Vec::new(),
    }
}

fn metadata_reply(owner: bool, metadata: serde_json::Value) -> Reply {
    let mut reply = session("synthetic-jam", owner);
    let mut root: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    root.as_object_mut()
        .unwrap()
        .extend(metadata.as_object().unwrap().clone());
    reply.body = serde_json::to_vec(&root).unwrap();
    reply
}

#[tokio::test(start_paused = true)]
async fn metadata_is_ephemeral_without_changing_authority_or_expiry() {
    let fake = Fake::new(vec![metadata_reply(
        false,
        serde_json::json!({
            "session_owner_id":"synthetic-account",
            "session_members":[{"id":"synthetic-account","display_name":"Named host"}],
            "join_session_url":"https://open.spotify.com/socialsession/Synthetic08"
        }),
    )]);
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    assert!(
        matches!(kernel.current().await.unwrap(), Membership::Present(p) if !p.is_session_owner)
    );
    let snapshot = kernel.snapshot();
    let deadline = snapshot.fresh_until;
    assert_eq!(snapshot.metadata.members.as_ref().unwrap().len(), 1);
    assert!(matches!(
        kernel.end("synthetic-jam", EndIntent::ForEveryone).await,
        Err(Error::NotOwner)
    ));
    assert_eq!(kernel.snapshot().fresh_until, deadline);
    tokio::time::advance(FRESH_FOR).await;
    assert!(kernel.snapshot().metadata.members.is_none());
    assert!(kernel.state().metadata.invitation.is_none());
    assert_eq!(kernel.snapshot().fresh_until, deadline);
    assert_eq!(fake.writes(), 0);
}

#[tokio::test]
async fn metadata_snapshots_do_not_merge_omission_or_cross_account_boundaries() {
    let fake = Fake::new(vec![
        metadata_reply(
            true,
            serde_json::json!({"session_members":[{"id":"synthetic"}],"join_session_url":"https://open.spotify.com/socialsession/Synthetic08"}),
        ),
        metadata_reply(true, serde_json::json!({})),
        metadata_reply(true, serde_json::json!({"session_members":[]})),
    ]);
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    kernel.current().await.unwrap();
    assert!(kernel.snapshot().metadata.invitation.is_some());
    kernel.current().await.unwrap();
    assert!(kernel.snapshot().metadata.members.is_none());
    assert!(kernel.snapshot().metadata.invitation.is_none());
    kernel.current().await.unwrap();
    assert!(kernel.snapshot().metadata.members.unwrap().is_empty());
    fake.context.lock().unwrap().as_mut().unwrap().account = "other-synthetic-account".into();
    let snapshot = kernel.snapshot();
    assert!(snapshot.membership == Membership::Unknown);
    assert!(snapshot.metadata.members.is_none());
}

#[tokio::test]
async fn metadata_late_reply_cannot_restore_a_retired_binding() {
    let fake = Fake::new(vec![]);
    let (send, receive) = tokio::sync::oneshot::channel();
    fake.push(Step::Wait(receive));
    let kernel = Arc::new(Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields));
    let task = tokio::spawn({
        let kernel = Arc::clone(&kernel);
        async move { kernel.current().await }
    });
    while fake.count() == 0 {
        tokio::task::yield_now().await;
    }
    kernel.invalidate();
    send.send(metadata_reply(true, serde_json::json!({"session_members":[{"id":"synthetic"}],"join_session_url":"https://open.spotify.com/socialsession/Synthetic08"}))).ok().unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Stale)));
    assert!(kernel.snapshot().metadata.members.is_none());
    assert!(kernel.snapshot().metadata.invitation.is_none());
}

enum Step {
    Reply(Reply),
    Error(Error),
    Wait(tokio::sync::oneshot::Receiver<Reply>),
    Preparing(tokio::sync::oneshot::Receiver<()>, Reply),
    Pending,
}

struct Fake {
    context: Mutex<Option<Context>>,
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<Operation>>,
}

impl Fake {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            context: Mutex::new(Some(context())),
            steps: Mutex::new(replies.into_iter().map(Step::Reply).collect()),
            calls: Mutex::new(Vec::new()),
        })
    }
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn writes(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| **op != Operation::Current)
            .count()
    }
}

impl Transport for Fake {
    fn context(&self) -> Result<Context, Error> {
        self.context.lock().unwrap().clone().ok_or(Error::Stale)
    }
    fn send<'a>(
        &'a self,
        dispatch: Dispatch,
        expected: &'a Context,
        permit: Check<'a>,
    ) -> Response<'a> {
        Box::pin(async move {
            permit()?;
            if self.context()? != *expected {
                return Err(Error::Stale);
            }
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected dispatch");
            if let Step::Preparing(receiver, reply) = step {
                receiver.await.map_err(|_| Error::Transport)?;
                permit()?;
                dispatch.check_freshness()?;
                dispatch.mark_emitted();
                self.calls.lock().unwrap().push(dispatch.operation);
                return Ok(reply);
            }
            dispatch.check_freshness()?;
            dispatch.mark_emitted();
            self.calls.lock().unwrap().push(dispatch.operation);
            match step {
                Step::Reply(reply) => Ok(reply),
                Step::Error(error) => Err(error),
                Step::Wait(receiver) => receiver.await.map_err(|_| Error::Transport),
                Step::Pending => std::future::pending().await,
                Step::Preparing(_, _) => unreachable!(),
            }
        })
    }
}

fn kernel(fake: &Arc<Fake>) -> Kernel<Fake> {
    Kernel::new(Arc::clone(fake), Profile::ObservedFourFields)
}

#[tokio::test]
async fn diagnostic_current_preserves_parser_results_and_reports_validation_boundary() {
    use super::diagnostic::UrlField;
    let mut mime = session("private-sentinel", true);
    mime.content_type = Some(b"application/x-protobuf".to_vec());
    let mut json = session("private-sentinel", true);
    json.body = br#"{"secret":"private-sentinel","secret":0}"#.to_vec();
    let mut projection = session("private-sentinel", true);
    projection.body = br#"{"session_id":"private-sentinel"}"#.to_vec();
    let cases = [
        (status(404), Schema::None, None, UrlField::NotObserved),
        (
            session("private-sentinel", false),
            Schema::None,
            None,
            UrlField::Absent,
        ),
        (
            status(405),
            Schema::None,
            Some(Error::Http(405)),
            UrlField::NotObserved,
        ),
        (
            mime,
            Schema::Mime,
            Some(Error::InvalidResponse),
            UrlField::NotObserved,
        ),
        (
            json,
            Schema::JsonStructure,
            Some(Error::InvalidResponse),
            UrlField::NotObserved,
        ),
        (
            metadata_reply(true, serde_json::json!({"session_members":null})),
            Schema::Metadata,
            Some(Error::InvalidResponse),
            UrlField::NotObserved,
        ),
        (
            projection,
            Schema::Projection,
            Some(Error::InvalidResponse),
            UrlField::Absent,
        ),
    ];
    for (reply, schema, expected_error, url_field) in cases {
        let ordinary = Reply {
            status: reply.status,
            content_type: reply.content_type.clone(),
            body: reply.body.clone(),
        };
        let http = reply.status;
        let fake = Fake::new(vec![reply]);
        let probe = Probe::default();
        let result = kernel(&fake)
            .run_observed(Operation::Current, Some(&probe))
            .await;
        let control = Fake::new(vec![ordinary]);
        let baseline = kernel(&control).current().await;
        assert_eq!(result.as_ref().err().copied(), expected_error);
        assert!(result == baseline);
        assert_eq!(probe.snapshot().schema, schema);
        assert_eq!(probe.snapshot().http, Some(http));
        assert_eq!(probe.snapshot().url_field, url_field);
        assert_eq!((fake.count(), control.count(), fake.writes()), (1, 1, 0));
        assert!(!probe.snapshot().to_string().contains("private-sentinel"));
    }
}

#[tokio::test(start_paused = true)]
async fn diagnostic_invitation_classifies_only_the_parsed_field_without_changing_state_or_requests()
{
    use super::diagnostic::UrlField::*;
    use serde_json::json;
    let cases = [
        (json!({}), Absent, false, false),
        (json!({"join_session_url":""}), Blank, false, false),
        (json!({"join_session_url":"  "}), Blank, false, false),
        (
            json!({"join_session_url":"https://open.spotify.com/socialsession/PrivateSentinel"}),
            Accepted,
            true,
            false,
        ),
        (
            json!({"joinSessionUrl":"spotify://sessions/join/PrivateSentinel"}),
            Accepted,
            true,
            false,
        ),
        (
            json!({"join_session_url":"https://open.spotify.com/sessions/join/PrivateSentinel"}),
            Accepted,
            true,
            false,
        ),
        (
            json!({"join_session_url":"https://open.spotify.com/sessions/join/PrivateSentinel?si=x"}),
            FormatRejected,
            false,
            false,
        ),
        (
            json!({"join_session_url":"https://untrusted.invalid/PrivateSentinel"}),
            FormatRejected,
            false,
            false,
        ),
        (
            json!({"join_session_url":" https://open.spotify.com/socialsession/PrivateSentinel"}),
            FormatRejected,
            false,
            false,
        ),
        (
            json!({"join_session_url":"https://open.spotify.com/socialsession/PrivateSentinel?unknown=x"}),
            FormatRejected,
            false,
            false,
        ),
        (json!({"join_session_url":null}), TypeRejected, false, true),
        (json!({"join_session_url":false}), TypeRejected, false, true),
        (json!({"join_session_url":[]}), TypeRejected, false, true),
        (
            json!({"join_session_url":"x","joinSessionUrl":"x"}),
            AliasConflict,
            false,
            true,
        ),
        (
            json!({"join_session_url":"x".repeat(2049)}),
            BoundsOrControlRejected,
            false,
            true,
        ),
        (
            json!({"join_session_url":"\n"}),
            BoundsOrControlRejected,
            false,
            true,
        ),
        // A prior metadata failure must not inspect the invitation separately.
        (
            json!({"session_members":null,"join_session_url":null}),
            NotObserved,
            false,
            true,
        ),
        (
            json!({"session_owner_id":false,"join_session_url":"x"}),
            NotObserved,
            false,
            true,
        ),
        (
            json!({"join_session_uri":"spotify://sessions/join/PrivateSentinel"}),
            Absent,
            false,
            false,
        ),
    ];
    for (fields, expected, has_invitation, rejected) in cases {
        let make_reply = || metadata_reply(true, fields.clone());
        let fake = Fake::new(vec![make_reply()]);
        let control = Fake::new(vec![make_reply()]);
        let observed = kernel(&fake);
        let ordinary = kernel(&control);
        let probe = Probe::default();
        let result = observed
            .run_observed(Operation::Current, Some(&probe))
            .await;
        assert!(result == ordinary.current().await);
        assert_eq!(result.is_err(), rejected);
        assert_eq!(probe.snapshot().url_field, expected);
        assert_eq!(probe.snapshot().http, Some(200));
        let actual = observed.snapshot();
        let baseline = ordinary.snapshot();
        assert!(actual.metadata == baseline.metadata);
        assert!(actual.membership == baseline.membership);
        assert_eq!(actual.metadata.invitation.is_some(), has_invitation);
        assert_eq!(actual.fresh_until, baseline.fresh_until);
        assert_eq!(actual.reconcile_required, baseline.reconcile_required);
        assert_eq!(actual.error, baseline.error);
        assert_eq!((fake.count(), control.count(), fake.writes()), (1, 1, 0));
        let output = probe.snapshot().to_string();
        assert!(output.contains("source=deliberate_current"));
        assert!(!output.contains("PrivateSentinel"));
    }
}

#[tokio::test]
async fn diagnostic_invitation_late_response_cannot_restore_retired_context() {
    let fake = Fake::new(vec![]);
    let (send, receive) = tokio::sync::oneshot::channel();
    fake.push(Step::Wait(receive));
    let kernel = Arc::new(kernel(&fake));
    let probe = Probe::default();
    let task = tokio::spawn({
        let kernel = Arc::clone(&kernel);
        let probe = probe.clone();
        async move { kernel.run_observed(Operation::Current, Some(&probe)).await }
    });
    while fake.count() == 0 {
        tokio::task::yield_now().await;
    }
    probe.retire(lifecycle::Cause::Context);
    kernel.invalidate();
    send.send(metadata_reply(
        true,
        serde_json::json!({
            "join_session_url":"https://open.spotify.com/socialsession/PrivateSentinel"
        }),
    ))
    .ok()
    .unwrap();
    assert_eq!(task.await.unwrap().err(), Some(Error::Stale));
    assert_eq!(
        probe.snapshot().url_field,
        diagnostic::UrlField::NotObserved
    );
    assert!(kernel.snapshot().metadata.invitation.is_none());
    assert!(kernel.snapshot().membership == Membership::Unknown);
    assert_eq!((fake.count(), fake.writes()), (1, 0));
}

#[tokio::test]
async fn diagnostic_headers_survive_body_failure_limit_and_context_refusal() {
    use http_body_util::{Full, Limited};
    // Limited emits a real body frame error, without a network or new transport.
    let probe = Probe::default();
    let response = http::Response::builder()
        .status(405)
        .body(Limited::new(Full::new(&b"private-sentinel"[..]), 0))
        .unwrap();
    let result = receive_reply(response, &|| Ok(()), Some(&probe)).await;
    probe.finish(Err(result.as_ref().err().copied().unwrap()));
    assert_eq!(result.err(), Some(Error::Transport));
    assert_eq!(
        probe.snapshot(),
        diagnostic::Diagnostic {
            class: diagnostic::Class::Failed(Stage::Body, Error::Transport),
            http: Some(405),
            schema: Schema::None,
            url_field: diagnostic::UrlField::NotObserved,
        }
    );
    let body = vec![b'x'; MAX_BODY + 1];
    let response = http::Response::builder()
        .status(404)
        .body(Full::new(body.as_slice()))
        .unwrap();
    let probe = Probe::default();
    assert_eq!(
        receive_reply(response, &|| Ok(()), Some(&probe))
            .await
            .err(),
        Some(Error::InvalidResponse)
    );
    assert_eq!(probe.snapshot().schema, Schema::BodyLimit);
    assert_eq!(probe.snapshot().http, Some(404));
    assert_eq!(
        probe.snapshot().url_field,
        diagnostic::UrlField::NotObserved
    );
    let response = http::Response::builder()
        .status(404)
        .body(Full::new(&b""[..]))
        .unwrap();
    let probe = Probe::default();
    assert_eq!(
        receive_reply(response, &|| Err(Error::Stale), Some(&probe))
            .await
            .err(),
        Some(Error::Stale)
    );
    assert_eq!(probe.snapshot().http, Some(404));
    probe.retire(lifecycle::Cause::Context);
    probe.status(200);
    assert_eq!(probe.snapshot().http, None);
    assert_eq!(probe.snapshot().schema, Schema::None);
    assert_eq!(
        probe.snapshot().url_field,
        diagnostic::UrlField::NotObserved
    );
}

#[tokio::test]
async fn diagnostic_prepared_refusal_is_not_reported_as_a_polled_transport() {
    let probe = Probe::default();
    probe.stage(Stage::Preparation);
    let dispatch = Dispatch {
        operation: Operation::Current,
        fresh_until: None,
        uncertainty: None,
        diagnostic: Some(probe.clone()),
    };
    let prepared: Result<std::future::Ready<Result<(), Error>>, Error> = Err(Error::Transport);
    let result = dispatch.poll_prepared(prepared, &|| Ok(())).await;
    probe.finish(
        result
            .as_ref()
            .map(|_| &Membership::Unknown)
            .map_err(|e| *e),
    );
    assert_eq!(
        probe.snapshot().class,
        diagnostic::Class::Failed(Stage::Preparation, Error::Transport)
    );
    assert_eq!(probe.snapshot().http, None);
}

#[tokio::test]
async fn local_http_refusal_before_future_does_not_latch_or_poll_http() {
    use std::sync::atomic::AtomicUsize;
    struct LocalRefusal {
        client: librespot_core::http_client::HttpClient,
        attempts: AtomicUsize,
        http_polls: AtomicUsize,
    }
    impl Transport for LocalRefusal {
        fn context(&self) -> Result<Context, Error> {
            Ok(context())
        }
        fn send<'a>(
            &'a self,
            dispatch: Dispatch,
            _: &'a Context,
            permit: Check<'a>,
        ) -> Response<'a> {
            Box::pin(async move {
                permit()?;
                if dispatch.operation == Operation::Current {
                    return Ok(if self.attempts.load(Ordering::SeqCst) < 2 {
                        status(404)
                    } else {
                        session("synthetic-joined", false)
                    });
                }
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    let request = Request::builder()
                        .uri("https://example.invalid/synthetic")
                        .body(Vec::<u8>::new())
                        .unwrap();
                    let prepared = self
                        .client
                        .request_fut(request.map(Into::into))
                        .map_err(|_| Error::Transport)
                        .map(|future| {
                            std::future::poll_fn(
                                move |_| -> std::task::Poll<Result<Reply, Error>> {
                                    let _unpolled_http = &future;
                                    self.http_polls.fetch_add(1, Ordering::SeqCst);
                                    panic!("a synchronous local refusal must never poll HTTP");
                                },
                            )
                        });
                    // The same first-poll boundary as SessionTransport.
                    dispatch.poll_prepared(prepared, permit).await
                } else {
                    dispatch
                        .poll_prepared(Ok(async { Ok(session("synthetic-joined", false)) }), permit)
                        .await
                }
            })
        }
    }
    let client = librespot_core::http_client::HttpClient::new(None);
    let mut refused = false;
    for _ in 0..1000 {
        let request = Request::builder()
            .uri("https://example.invalid/synthetic")
            .body(Vec::<u8>::new())
            .unwrap();
        match client.request_fut(request.map(Into::into)) {
            Ok(future) => drop(future), // Construction alone never sends HTTP.
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    assert!(
        refused,
        "the real pin's local limiter must reject synchronously"
    );
    let transport = Arc::new(LocalRefusal {
        client,
        attempts: AtomicUsize::new(0),
        http_polls: AtomicUsize::new(0),
    });
    let kernel = Kernel::new(Arc::clone(&transport), Profile::ObservedFourFields);
    kernel.current().await.unwrap();
    assert_eq!(
        kernel
            .join(link(), JoinIntent::ListenAndControlRequested)
            .await
            .err(),
        Some(Error::Transport)
    );
    assert!(!kernel.write_uncertain.load(Ordering::SeqCst));
    assert!(kernel.snapshot().reconcile_required);
    assert_eq!(
        kernel
            .join(link(), JoinIntent::ListenAndControlRequested)
            .await
            .err(),
        Some(Error::ReconcileRequired)
    );
    kernel.current().await.unwrap();
    assert!(!kernel.snapshot().reconcile_required);
    assert!(
        kernel
            .join(link(), JoinIntent::ListenAndControlRequested)
            .await
            .is_ok()
    );
    assert_eq!(transport.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(transport.http_polls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn prepared_future_checks_before_poll_and_retains_possible_emission_on_cancel() {
    use std::sync::atomic::AtomicUsize;
    let fence = Arc::new(AtomicBool::new(false));
    let polls = AtomicUsize::new(0);
    for expired in [false, true] {
        let dispatch = Dispatch {
            diagnostic: None,
            operation: Operation::Join(link()),
            fresh_until: Some(Instant::now() + if expired { Duration::ZERO } else { FRESH_FOR }),
            uncertainty: Some(Arc::clone(&fence)),
        };
        let future = std::future::poll_fn(|_| -> std::task::Poll<Result<(), Error>> {
            polls.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Pending
        });
        let check = || if expired { Ok(()) } else { Err(Error::Stale) };
        assert_eq!(
            dispatch.poll_prepared(Ok(future), &check).await.err(),
            Some(if expired {
                Error::SnapshotExpired
            } else {
                Error::Stale
            })
        );
        assert!(!fence.load(Ordering::SeqCst));
    }
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    let dispatch = Dispatch {
        diagnostic: None,
        operation: Operation::Join(link()),
        fresh_until: None,
        uncertainty: Some(Arc::clone(&fence)),
    };
    let future = std::future::poll_fn(|_| -> std::task::Poll<Result<(), Error>> {
        polls.fetch_add(1, Ordering::SeqCst);
        std::task::Poll::Pending
    });
    let check = || Ok(());
    let mut operation = Box::pin(dispatch.poll_prepared(Ok(future), &check));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut operation)
            .await
            .is_err()
    );
    drop(operation);
    assert!(fence.load(Ordering::SeqCst));
    assert!(polls.load(Ordering::SeqCst) > 0);
}

#[test]
fn request_preparation_is_exact_and_encodes_context_without_an_http_helper() {
    let context = context();
    for (operation, method, path, extra) in [
        (
            Operation::Current,
            Method::GET,
            "/social-connect/v2/sessions/current",
            vec![],
        ),
        (
            Operation::Create,
            Method::GET,
            "/social-connect/v2/sessions/current_or_new",
            vec![
                ("activate", "true"),
                ("local_device_id", context.device.as_str()),
                ("type", "REMOTE"),
            ],
        ),
        (
            Operation::End("jam_1-A".into()),
            Method::DELETE,
            "/social-connect/v3/sessions/jam_1-A",
            vec![("local_device_id", context.device.as_str())],
        ),
        (
            Operation::Join(link()),
            Method::POST,
            "/social-connect/v2/sessions/join/Synthetic123",
            vec![
                ("playback_control", "listen_and_control"),
                ("join_type", "deeplinking"),
                ("local_device_id", context.device.as_str()),
            ],
        ),
    ] {
        let request = prepare_request(
            &operation,
            &context,
            "https://example.test/",
            "E&C",
            12,
            "Bearer synthetic",
            Some("synthetic-client-token"),
        )
        .unwrap();
        assert_eq!(*request.method(), method);
        let url = reqwest::Url::parse(&request.uri().to_string()).unwrap();
        assert_eq!(url.path(), path);
        let mut expected = extra
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect::<Vec<_>>();
        expected.extend([
            ("product".into(), "0".into()),
            ("country".into(), "E&C".into()),
            ("salt".into(), "12".into()),
        ]);
        assert_eq!(url.query_pairs().into_owned().collect::<Vec<_>>(), expected);
        assert!(request.body().is_empty());
        let headers = request.headers();
        assert_eq!(headers.len(), 5);
        assert_eq!(headers["authorization"], "Bearer synthetic");
        assert_eq!(headers["spotify-connection-id"], context.connection);
        assert_eq!(headers["content-length"], "0");
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(headers["client-token"], "synthetic-client-token");
        assert!(!headers.contains_key("content-type"));
    }
    let request = prepare_request(
        &Operation::Current,
        &context,
        "https://example.test",
        "EC",
        1,
        "Bearer synthetic",
        None,
    )
    .unwrap();
    assert!(!request.headers().contains_key("client-token"));
    for id in ["", "x/y", "..", "?", "a\n", "é", "a%2fb"] {
        assert_eq!(
            prepare_request(
                &Operation::End(id.into()),
                &context,
                "https://example.test",
                "EC",
                1,
                "Bearer synthetic",
                None
            )
            .err(),
            Some(Error::InvalidId)
        );
    }
}

fn link() -> Link {
    Link::parse("https://open.spotify.com/socialsession/Synthetic123?si=fixture&utm_source=test")
        .unwrap()
}

async fn join(kernel: &Kernel<Fake>) -> Result<Membership, Error> {
    kernel
        .join(link(), JoinIntent::ListenAndControlRequested)
        .await
}

#[test]
fn join_link_is_exact_bounded_and_discards_only_valid_tracking() {
    assert!(link() == Link::parse("https://open.spotify.com/socialsession/Synthetic123").unwrap());
    for value in [
        "",
        "http://open.spotify.com/socialsession/A",
        "https://spotify.link/A",
        "https://open.spotify.com:443/socialsession/A",
        "https://open.spotify.com.evil.test/socialsession/A",
        "https://user@open.spotify.com/socialsession/A",
        "https://OPEN.spotify.com/socialsession/A",
        "https://open.spotify.com/socialsession/",
        "https://open.spotify.com/socialsession/A/",
        "https://open.spotify.com/socialsession/A#f",
        "https://open.spotify.com/socialsession/A%2fB",
        " https://open.spotify.com/socialsession/A",
        "https://open.spotify.com/socialsession/A\n",
        "https://open.spotify.com/socialsession/A?",
        "https://open.spotify.com/socialsession/A?si=x&si=y",
        "https://open.spotify.com/socialsession/A?si=",
        "https://open.spotify.com/socialsession/A?unknown=x",
        "https://open.spotify.com/socialsession/A?si=%41",
        "https://open.spotify.com/socialsession/A?si=a+b",
        "https://open.spotify.com/socialsession/A?utm_source=x&utm_source=y",
    ] {
        assert_eq!(Link::parse(value).err(), Some(Error::InvalidLink));
    }
    assert_eq!(
        Link::parse(&format!(
            "https://open.spotify.com/socialsession/{}",
            "a".repeat(1025)
        ))
        .err(),
        Some(Error::InvalidLink)
    );
    assert_eq!(
        Link::parse(&format!(
            "https://open.spotify.com/socialsession/A?si={}",
            "a".repeat(257)
        ))
        .err(),
        Some(Error::InvalidLink)
    );
}

#[tokio::test(start_paused = true)]
async fn join_requires_absence_freshness_and_no_reconcile_or_pending() {
    let fake = Fake::new(vec![]);
    let kernel = kernel(&fake);
    assert_eq!(join(&kernel).await.err(), Some(Error::ReconcileRequired));
    assert_eq!(fake.count(), 0);
    for owner in [true, false] {
        fake.push(Step::Reply(session("existing", owner)));
        kernel.current().await.unwrap();
        assert_eq!(join(&kernel).await.err(), Some(Error::NotAbsent));
    }
    fake.push(Step::Reply(status(404)));
    kernel.current().await.unwrap();
    tokio::time::advance(FRESH_FOR).await;
    assert_eq!(join(&kernel).await.err(), Some(Error::SnapshotExpired));
    fake.push(Step::Pending);
    let mut read = Box::pin(kernel.current());
    tokio::select! { _ = &mut read => panic!("pending"), _ = tokio::task::yield_now() => () }
    assert_eq!(join(&kernel).await.err(), Some(Error::Busy));
    assert_eq!(fake.writes(), 0);
}

#[tokio::test]
async fn join_correlates_strict_member_reply_and_current_without_mode_inference() {
    let fake = Fake::new(vec![
        status(404),
        session("joined", false),
        session("joined", false),
    ]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    assert!(
        matches!(join(&kernel).await, Ok(Membership::Present(p)) if !p.is_session_owner && p.is_listening && !p.is_controlling)
    );
    assert_eq!(fake.writes(), 1);
    assert_eq!(join(&kernel).await.err(), Some(Error::NotAbsent));
    assert_eq!(fake.count(), 3);
    for followup in [
        session("different", false),
        session("joined", true),
        status(404),
        status(200),
    ] {
        let fake = Fake::new(vec![status(404), session("joined", false), followup]);
        let kernel = self::kernel(&fake);
        kernel.current().await.unwrap();
        assert!(join(&kernel).await.is_err());
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert!(kernel.snapshot().reconcile_required);
    }
    for reply in [session("joined", true), status(200), status(204)] {
        let fake = Fake::new(vec![status(404), reply]);
        let kernel = self::kernel(&fake);
        kernel.current().await.unwrap();
        assert!(join(&kernel).await.is_err());
        assert_eq!(fake.count(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn ambiguous_join_cannot_be_replayed_after_current_404_or_invalidation() {
    for step in [
        Step::Pending,
        Step::Error(Error::Transport),
        Step::Reply(status(500)),
        Step::Reply(status(200)),
    ] {
        let fake = Fake::new(vec![status(404)]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        fake.push(step);
        assert!(join(&kernel).await.is_err());
        fake.push(Step::Reply(status(404)));
        assert!(kernel.current().await.unwrap() == Membership::Absent);
        assert!(kernel.snapshot().reconcile_required);
        assert_eq!(kernel.snapshot().error, Some(Error::WriteUncertain));
        assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
        assert_eq!(
            kernel.create(CreateIntent::Requested).await.err(),
            Some(Error::WriteUncertain)
        );
        kernel.invalidate();
        fake.push(Step::Reply(status(404)));
        kernel.current().await.unwrap();
        assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
        assert_eq!(fake.writes(), 1);
    }
}

#[tokio::test]
async fn rejected_join_needs_deliberate_current_then_a_new_gesture() {
    for code in [400, 403, 404, 405, 429] {
        let fake = Fake::new(vec![
            status(404),
            status(code),
            status(404),
            session("joined", false),
            session("joined", false),
        ]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        assert_eq!(join(&kernel).await.err(), Some(Error::Http(code)));
        assert_eq!(fake.count(), 2);
        assert_eq!(join(&kernel).await.err(), Some(Error::ReconcileRequired));
        kernel.current().await.unwrap();
        assert!(join(&kernel).await.is_ok());
        assert_eq!(fake.writes(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn join_preparation_expiry_context_loss_and_late_replies_are_fenced() {
    for preparing in [true, false] {
        for change in 0..6 {
            let fake = Fake::new(vec![status(404)]);
            let kernel = kernel(&fake);
            kernel.current().await.unwrap();
            let (release, wait) = tokio::sync::oneshot::channel();
            let (reply, response) = tokio::sync::oneshot::channel();
            fake.push(if preparing {
                Step::Preparing(wait, session("joined", false))
            } else {
                Step::Wait(response)
            });
            let mut task = Box::pin(join(&kernel));
            tokio::select! { _ = &mut task => panic!("pending"), _ = tokio::task::yield_now() => () }
            assert_eq!(join(&kernel).await.err(), Some(Error::Busy));
            match change {
                0 => tokio::time::advance(FRESH_FOR).await,
                1 => *fake.context.lock().unwrap() = None,
                2 => {
                    fake.context.lock().unwrap().as_mut().unwrap().connection = "replacement".into()
                }
                3 => fake.context.lock().unwrap().as_mut().unwrap().generation += 1,
                4 => fake.context.lock().unwrap().as_mut().unwrap().account = "replacement".into(),
                _ => kernel.invalidate(),
            }
            if preparing {
                release.send(()).ok();
            } else {
                reply.send(session("joined", false)).ok();
            }
            // Expiry after emission cannot undo a request. It still needs a Current.
            if !preparing && change == 0 {
                fake.push(Step::Reply(session("joined", false)));
            }
            let result = task.await;
            if !preparing && change == 0 {
                assert!(result.is_ok());
            } else {
                assert_eq!(
                    result.err(),
                    Some(if change == 0 {
                        Error::SnapshotExpired
                    } else {
                        Error::Stale
                    })
                );
                assert!(kernel.snapshot().membership == Membership::Unknown);
            }
            assert_eq!(fake.writes(), usize::from(!preparing));
        }
    }
}

#[test]
fn profile_requires_four_typed_fields_and_rejects_duplicates_and_budgets() {
    let good =
        r#"{"sessionId":"s","isSessionOwner":true,"isListening":false,"isControlling":true}"#;
    assert!(matches!(projection::read(good.as_bytes()), Ok(p) if p.is_session_owner));
    for bad in [
        "null",
        r#"["s",true,true,true]"#,
        "[]",
        "{}",
        r#"{"session_id":"s","is_session_owner":true,"is_listening":true}"#,
        r#"{"session_id":"s","is_session_owner":null,"is_listening":true,"is_controlling":true}"#,
        r#"{"session_id":"s","is_session_owner":1,"is_listening":true,"is_controlling":true}"#,
        r#"{"session_id":" ","is_session_owner":true,"is_listening":true,"is_controlling":true}"#,
        r#"{"session_id":"s","is_session_owner":true,"isSessionOwner":true,"is_listening":true,"is_controlling":true}"#,
        r#"{"session_id":"s","session_id":"s","is_session_owner":true,"is_listening":true,"is_controlling":true}"#,
    ] {
        assert_eq!(
            projection::read(bad.as_bytes()).err(),
            Some(Error::InvalidResponse)
        );
    }
    for metadata in [
        r#"{"a":1,"a":2}"#.to_owned(),
        format!("{}0{}", "[".repeat(33), "]".repeat(33)),
        format!("[{}]", vec!["0"; 4096].join(",")),
        format!("\"{}\"", "a".repeat(MAX_BODY)),
    ] {
        let body = format!("{},\"ignored\":{metadata}}}", &good[..good.len() - 1]);
        assert_eq!(
            projection::read(body.as_bytes()).err(),
            Some(Error::InvalidResponse)
        );
    }
    let ignored = format!(
        "{},\"ignored\":{{\"new_field\":[1,null,true]}}}}",
        &good[..good.len() - 1]
    );
    assert!(projection::read(ignored.as_bytes()).is_ok());
    for content_type in [
        Some(b"".to_vec()),
        Some(b"application/x-protobuf".to_vec()),
        Some(vec![255]),
    ] {
        let mut reply = session("s", true);
        reply.content_type = content_type;
        assert_eq!(
            membership(reply, &Profile::ObservedFourFields).err(),
            Some(Error::InvalidResponse)
        );
    }
    let mut reply = session("s", true);
    reply.content_type = Some(b"application/json; charset=utf-8".to_vec());
    assert!(membership(reply, &Profile::ObservedFourFields).is_ok());
    let mut reply = session("s", true);
    reply.status = 201;
    assert_eq!(
        membership(reply, &Profile::ObservedFourFields).err(),
        Some(Error::InvalidResponse)
    );
}

#[tokio::test]
async fn construction_is_passive_and_create_is_one_mutation_with_owner_verification() {
    let fake = Fake::new(vec![
        status(404),
        session("host", true),
        session("host", true),
    ]);
    let kernel = kernel(&fake);
    fn assert_send<T: Send>(_: T) {}
    assert_send(kernel.current());
    assert_send(kernel.create(CreateIntent::Requested));
    assert_send(kernel.end("host", EndIntent::ForEveryone));
    assert_eq!(fake.count(), 0);
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::ReconcileRequired)
    );
    assert!(kernel.current().await.unwrap() == Membership::Absent);
    assert!(
        matches!(kernel.create(CreateIntent::Requested).await, Ok(Membership::Present(p)) if p.is_session_owner)
    );
    assert_eq!(fake.writes(), 1);
    assert!(
        *fake.calls.lock().unwrap() == [Operation::Current, Operation::Create, Operation::Current]
    );
    assert!(!kernel.snapshot().reconcile_required);
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::NotAbsent)
    );
    assert_eq!(fake.writes(), 1);
}

#[tokio::test]
async fn end_requires_the_confirmed_owner_and_only_reports_own_absence() {
    let fake = Fake::new(vec![
        session("host", false),
        session("host", true),
        status(204),
        status(404),
    ]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    assert_eq!(
        kernel.end("host", EndIntent::ForEveryone).await.err(),
        Some(Error::NotOwner)
    );
    kernel.current().await.unwrap();
    assert_eq!(
        kernel.end("other", EndIntent::ForEveryone).await.err(),
        Some(Error::NotOwner)
    );
    assert_eq!(
        kernel.end("bad/id", EndIntent::ForEveryone).await.err(),
        Some(Error::InvalidId)
    );
    assert!(kernel.end("host", EndIntent::ForEveryone).await.unwrap() == Membership::Absent);
    assert_eq!(fake.writes(), 1);
    assert!(kernel.snapshot().membership == Membership::Absent);
    for code in [403, 405, 429, 500] {
        let rejected = Fake::new(vec![session("host", true), status(code)]);
        let kernel = self::kernel(&rejected);
        kernel.current().await.unwrap();
        assert_eq!(
            kernel.end("host", EndIntent::ForEveryone).await.err(),
            Some(Error::Http(code))
        );
        assert_eq!(rejected.count(), 2);
        assert_eq!(rejected.writes(), 1);
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert!(kernel.snapshot().reconcile_required);
    }
}

#[tokio::test(start_paused = true)]
async fn expired_current_cannot_authorize_a_write() {
    let fake = Fake::new(vec![status(404), session("host", true)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    tokio::time::advance(FRESH_FOR).await;
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::SnapshotExpired)
    );
    kernel.current().await.unwrap();
    tokio::time::advance(FRESH_FOR).await;
    assert_eq!(
        kernel.end("host", EndIntent::ForEveryone).await.err(),
        Some(Error::SnapshotExpired)
    );
    assert_eq!(fake.writes(), 0);
}

#[tokio::test]
async fn rejected_and_uncertain_writes_never_verify_retry_or_publish_success() {
    for step in [
        Step::Reply(status(403)),
        Step::Reply(status(405)),
        Step::Reply(status(429)),
        Step::Reply(status(500)),
        Step::Error(Error::Transport),
        Step::Reply(status(200)),
    ] {
        let fake = Fake::new(vec![status(404)]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        fake.push(step);
        assert!(kernel.create(CreateIntent::Requested).await.is_err());
        assert_eq!(fake.count(), 2, "no verification read after failure");
        let snapshot = kernel.snapshot();
        assert!(snapshot.membership == Membership::Unknown);
        assert!(snapshot.reconcile_required && snapshot.error.is_some());
        let uncertain = !matches!(snapshot.error, Some(Error::Http(403 | 405 | 429)));
        assert_eq!(
            kernel.create(CreateIntent::Requested).await.err(),
            Some(if uncertain {
                Error::WriteUncertain
            } else {
                Error::ReconcileRequired
            })
        );
        assert_eq!(fake.writes(), 1);
        fake.push(Step::Reply(status(404)));
        kernel.current().await.unwrap();
        assert_eq!(kernel.snapshot().reconcile_required, uncertain);
        if uncertain {
            assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
            assert_eq!(fake.writes(), 1);
        }
    }
}

#[tokio::test]
async fn verification_must_match_the_created_owner_or_confirm_departure() {
    for followup in [
        session("different", true),
        session("host", false),
        status(404),
        status(403),
    ] {
        let fake = Fake::new(vec![status(404), session("host", true), followup]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        assert!(kernel.create(CreateIntent::Requested).await.is_err());
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert!(kernel.snapshot().reconcile_required);
    }
    for followup in [session("host", true), status(403), status(200)] {
        let fake = Fake::new(vec![session("host", true), status(204), followup]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        assert!(kernel.end("host", EndIntent::ForEveryone).await.is_err());
        assert_eq!(fake.writes(), 1);
        assert!(kernel.snapshot().reconcile_required);
    }
    let fake = Fake::new(vec![status(404), session("host", false)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::VerificationFailed)
    );
    assert_eq!(
        fake.count(),
        2,
        "nonowner create reply cannot authorize verification success"
    );
}

#[tokio::test(start_paused = true)]
async fn timeout_and_cancellation_require_explicit_reconciliation() {
    for cancel in [false, true] {
        let fake = Fake::new(vec![status(404)]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        fake.push(Step::Pending);
        if cancel {
            let mut future = Box::pin(kernel.create(CreateIntent::Requested));
            assert!(
                tokio::time::timeout(Duration::from_secs(1), &mut future)
                    .await
                    .is_err()
            );
            drop(future);
        } else {
            assert_eq!(
                kernel.create(CreateIntent::Requested).await.err(),
                Some(Error::Timeout)
            );
        }
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
        fake.push(Step::Reply(status(404)));
        kernel.current().await.unwrap();
        assert!(kernel.snapshot().reconcile_required);
        assert_eq!(
            kernel.create(CreateIntent::Requested).await.err(),
            Some(Error::WriteUncertain)
        );
        assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
        assert_eq!(fake.writes(), 1);
    }
}

#[tokio::test]
async fn uncertain_end_cannot_authorize_join_even_after_absent_current() {
    let fake = Fake::new(vec![session("host", true)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    fake.push(Step::Error(Error::Transport));
    assert_eq!(
        kernel.end("host", EndIntent::ForEveryone).await.err(),
        Some(Error::Transport)
    );
    fake.push(Step::Reply(status(404)));
    kernel.current().await.unwrap();
    assert_eq!(join(&kernel).await.err(), Some(Error::WriteUncertain));
    assert_eq!(fake.writes(), 1);
}

#[tokio::test]
async fn lifecycle_changes_fence_replies_and_snapshots_without_followup_io() {
    for change in 0..6 {
        let fake = Fake::new(vec![status(404)]);
        let kernel = kernel(&fake);
        kernel.current().await.unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        fake.push(Step::Wait(receiver));
        let mut future = Box::pin(kernel.create(CreateIntent::Requested));
        tokio::select! {
            result = &mut future => panic!("unexpected completion: {:?}", result.err()),
            _ = tokio::task::yield_now() => (),
        }
        assert_eq!(fake.writes(), 1);
        match change {
            0 => *fake.context.lock().unwrap() = None,
            1 => fake.context.lock().unwrap().as_mut().unwrap().generation += 1,
            2 => fake.context.lock().unwrap().as_mut().unwrap().account = "another".into(),
            3 => fake.context.lock().unwrap().as_mut().unwrap().device = "another".into(),
            4 => fake.context.lock().unwrap().as_mut().unwrap().connection = "another".into(),
            _ => kernel.invalidate(),
        }
        sender.send(session("host", true)).ok().unwrap();
        assert_eq!(future.await.err(), Some(Error::Stale));
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert!(kernel.snapshot().reconcile_required);
        assert_eq!(fake.count(), 2);
    }
}

#[tokio::test]
async fn gate_serializes_reads_and_writes_and_invalidation_discards_current() {
    let fake = Fake::new(vec![]);
    let kernel = kernel(&fake);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    fake.push(Step::Wait(receiver));
    let mut future = Box::pin(kernel.current());
    tokio::select! {
        result = &mut future => panic!("unexpected completion: {:?}", result.err()),
        _ = tokio::task::yield_now() => (),
    }
    assert_eq!(kernel.current().await.err(), Some(Error::Busy));
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::Busy)
    );
    kernel.invalidate();
    sender.send(status(404)).ok().unwrap();
    assert_eq!(future.await.err(), Some(Error::Stale));
    assert!(kernel.snapshot().membership == Membership::Unknown);
    assert_eq!(fake.count(), 1);
}

#[tokio::test]
async fn invalidation_during_preparation_prevents_the_mutating_dispatch() {
    let fake = Fake::new(vec![status(404)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    fake.push(Step::Preparing(receiver, session("host", true)));
    let mut future = Box::pin(kernel.create(CreateIntent::Requested));
    tokio::select! {
        result = &mut future => panic!("unexpected completion: {:?}", result.err()),
        _ = tokio::task::yield_now() => (),
    }
    kernel.invalidate();
    sender.send(()).unwrap();
    assert_eq!(future.await.err(), Some(Error::Stale));
    assert_eq!(fake.writes(), 0);
    assert!(kernel.snapshot().membership == Membership::Unknown);
}

#[tokio::test(start_paused = true)]
async fn freshness_must_hold_after_asynchronous_preparation() {
    let fake = Fake::new(vec![status(404)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    fake.push(Step::Preparing(receiver, session("host", true)));
    let mut future = Box::pin(kernel.create(CreateIntent::Requested));
    tokio::select! {
        result = &mut future => panic!("unexpected completion: {:?}", result.err()),
        _ = tokio::task::yield_now() => (),
    }
    tokio::time::advance(FRESH_FOR).await;
    sender.send(()).unwrap();
    assert_eq!(future.await.err(), Some(Error::SnapshotExpired));
    assert_eq!(fake.writes(), 0);
    assert!(kernel.snapshot().reconcile_required);
}

#[tokio::test]
async fn logout_also_clears_a_previously_published_owner_without_io() {
    let fake = Fake::new(vec![session("host", true)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    assert!(matches!(
        kernel.snapshot().membership,
        Membership::Present(_)
    ));
    *fake.context.lock().unwrap() = None;
    assert!(kernel.snapshot().membership == Membership::Unknown);
    assert_eq!(
        kernel.end("host", EndIntent::ForEveryone).await.err(),
        Some(Error::Stale)
    );
    assert_eq!(fake.writes(), 0);
}
