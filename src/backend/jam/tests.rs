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
                self.calls.lock().unwrap().push(dispatch.operation);
                return Ok(reply);
            }
            dispatch.check_freshness()?;
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
        assert_eq!(
            kernel.create(CreateIntent::Requested).await.err(),
            Some(Error::ReconcileRequired)
        );
        assert_eq!(fake.writes(), 1);
        fake.push(Step::Reply(status(404)));
        kernel.current().await.unwrap();
        assert!(!kernel.snapshot().reconcile_required);
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
    let fake = Fake::new(vec![status(404)]);
    let kernel = kernel(&fake);
    kernel.current().await.unwrap();
    fake.push(Step::Pending);
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::Timeout)
    );
    assert_eq!(fake.writes(), 1);
    assert!(kernel.snapshot().reconcile_required);
    fake.push(Step::Reply(status(404)));
    kernel.current().await.unwrap();
    fake.push(Step::Pending);
    let mut future = Box::pin(kernel.create(CreateIntent::Requested));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut future)
            .await
            .is_err()
    );
    drop(future);
    assert!(kernel.snapshot().membership == Membership::Unknown);
    assert_eq!(
        kernel.create(CreateIntent::Requested).await.err(),
        Some(Error::ReconcileRequired)
    );
    assert_eq!(fake.writes(), 2);
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
