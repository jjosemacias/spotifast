use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::api::{AccountId, ApiSource};
use crate::backend::{Worker, authorization_tests};

enum Step {
    Reply(Reply),
    Wait(tokio::sync::oneshot::Receiver<Reply>),
}
struct FakeState {
    context: Mutex<Context>,
    calls: Mutex<Vec<Operation>>,
    steps: Mutex<VecDeque<Step>>,
    factories: AtomicUsize,
}
#[derive(Clone)]
struct Fake(Arc<FakeState>);

impl Fake {
    fn new(account: &str) -> Self {
        Self(Arc::new(FakeState {
            context: Mutex::new(Context {
                account: account.into(),
                device: "synthetic-device".into(),
                connection: "connection-a".into(),
                generation: 0,
            }),
            calls: Mutex::new(Vec::new()),
            steps: Mutex::new(VecDeque::new()),
            factories: AtomicUsize::new(0),
        }))
    }
    fn push(&self, reply: Reply) {
        self.0.steps.lock().unwrap().push_back(Step::Reply(reply));
    }
    fn wait(&self) -> tokio::sync::oneshot::Sender<Reply> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.0.steps.lock().unwrap().push_back(Step::Wait(receiver));
        sender
    }
    fn calls(&self) -> usize {
        self.0.calls.lock().unwrap().len()
    }
    fn writes(&self) -> usize {
        self.0
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| **op != Operation::Current)
            .count()
    }
}

impl Transport for Fake {
    fn context(&self) -> Result<Context, Error> {
        Ok(self.0.context.lock().unwrap().clone())
    }
    fn send<'a>(
        &'a self,
        dispatch: Dispatch,
        context: &'a Context,
        permit: Check<'a>,
    ) -> Response<'a> {
        Box::pin(async move {
            permit()?;
            if self.context()? != *context {
                return Err(Error::Stale);
            }
            dispatch.check_freshness()?;
            self.0.calls.lock().unwrap().push(dispatch.operation);
            let step = self
                .0
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            match step {
                Step::Reply(reply) => Ok(reply),
                Step::Wait(receiver) => receiver.await.map_err(|_| Error::Transport),
            }
        })
    }
}

fn absent() -> Reply {
    Reply {
        status: 404,
        content_type: None,
        body: Vec::new(),
    }
}
fn empty(status: u16) -> Reply {
    Reply {
        status,
        content_type: None,
        body: Vec::new(),
    }
}
fn owner() -> Reply {
    Reply { status: 200, content_type: None,
        body: br#"{"session_id":"synthetic-jam","is_session_owner":true,"is_listening":false,"is_controlling":true}"#.to_vec() }
}

fn bind(worker: &mut Worker, fake: &Fake, account: &str) {
    let api = Arc::clone(&worker.api);
    let expected = account.to_owned();
    let verify: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        api.account()
            .is_some_and(|account| account.as_str() == expected)
    });
    let source = fake.clone();
    let make = factory(
        move || {
            source.0.factories.fetch_add(1, Ordering::SeqCst);
            Ok(source.clone())
        },
        Arc::clone(&verify),
        Arc::clone(&worker.background_session),
    );
    worker.jam.bind(
        *worker.session.borrow(),
        account.into(),
        make,
        verify,
        Some(fake.context().unwrap().connection),
        true,
    );
}

type Harness = (
    tokio::runtime::Runtime,
    Worker,
    Fake,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
    watch::Receiver<Option<Arc<Event>>>,
);

fn setup(name: &str) -> Harness {
    let (runtime, mut worker, _) = authorization_tests::worker(name);
    worker
        .api
        .install(ApiSource::Shared, AccountId::new("alice"))
        .unwrap();
    worker.signed_in = true;
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    worker.commands = commands;
    let (bridge, events) = Bridge::channel(Waker::default());
    worker.jam = bridge;
    let fake = Fake::new("alice");
    bind(&mut worker, &fake, "alice");
    (runtime, worker, fake, receiver, events)
}

fn request(worker: &mut Worker, action: Action) {
    let expected = worker.jam.gate.stamp();
    worker.request_jam(Request { expected, action });
}

async fn queued(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Completed {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await.unwrap() {
                Command::JamFinished(result) => return *result,
                // The offline authorization fixtures cancel before HTTP.
                Command::SignInEnded { .. } => {}
                _ => panic!("unexpected command"),
            }
        }
    })
    .await
    .unwrap()
}

async fn finish(worker: &mut Worker, receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) {
    worker.finish_jam(queued(receiver).await);
}

fn latest(events: &watch::Receiver<Option<Arc<Event>>>) -> Arc<Event> {
    events.borrow().clone().unwrap()
}

#[derive(Clone, Copy, Debug)]
enum IdentityTransition {
    Browser,
    Verification,
    RemovePersonal,
}

impl IdentityTransition {
    fn source(self) -> ApiSource {
        match self {
            Self::RemovePersonal => ApiSource::Personal,
            _ => ApiSource::Shared,
        }
    }

    fn apply(self, worker: &mut Worker) {
        match self {
            Self::Browser => {
                worker.sign_in_source_with_opener(self.source(), |_| Ok(()));
                assert!(matches!(
                    worker.api.state(self.source()),
                    crate::api::SessionState::Authorizing
                ));
            }
            Self::Verification => {
                worker.on_web_signed_in(self.source(), crate::auth::StoredToken::default());
                // Stop the spawned verification before its first poll. No HTTP.
                worker
                    .credentials
                    .invalidate(crate::credentials::Slot::Shared);
            }
            Self::RemovePersonal => worker.configure_personal_web_app(None),
        }
    }
}

fn consumer(
    worker: &Worker,
    events: watch::Receiver<Option<Arc<Event>>>,
) -> crate::backend::Backend {
    let mut backend = crate::backend::Backend::spawn(
        worker.dirs.clone(),
        worker.engine_config.clone(),
        None,
        Waker::default(),
        false,
    );
    backend.shutdown();
    backend.jam_events = events;
    backend.jam_gate = worker.jam.gate();
    backend
}

fn prepare_identity(worker: &mut Worker, transition: IdentityTransition, other_ready: bool) {
    if matches!(transition, IdentityTransition::RemovePersonal) {
        worker
            .api
            .install(ApiSource::Personal, AccountId::new("alice"))
            .unwrap();
        if !other_ready {
            worker
                .api
                .set_state(ApiSource::Shared, crate::api::SessionState::Authorizing);
        }
    } else if other_ready {
        worker
            .api
            .install(ApiSource::Personal, AccountId::new("alice"))
            .unwrap();
    }
}

fn restore_identity(worker: &mut Worker, transition: IdentityTransition) {
    worker.web_tokens = [None, None];
    worker.web_client_id = Some("personal".into());
    authorization_tests::verify(worker, transition.source(), "alice");
}

#[test]
fn last_verified_identity_loss_retires_before_same_account_returns_and_io_resumes() {
    let _port = authorization_tests::WEB_PORT.lock().unwrap();
    for transition in [
        IdentityTransition::Browser,
        IdentityTransition::Verification,
        IdentityTransition::RemovePersonal,
    ] {
        let (runtime, mut worker, fake, mut commands, events) =
            setup(&format!("jam-loss-{transition:?}"));
        prepare_identity(&mut worker, transition, false);
        let mut backend = consumer(&worker, events);
        runtime.block_on(async {
            let release = fake.wait();
            request(&mut worker, Action::Current);
            tokio::task::yield_now().await;
            let old = backend.jam_stamp();
            transition.apply(&mut worker);
            assert!(worker.api.account().is_none());
            // Consume Unknown while there is no verified account.
            let invalidated = backend
                .poll_jam()
                .expect("identity loss must reach the consumer");
            assert_eq!(invalidated.phase, Phase::Invalidated(Cause::Account));
            assert!(invalidated.snapshot.membership == Membership::Unknown);
            restore_identity(&mut worker, transition);
            assert!(
                !worker.jam.gate.accepts(old),
                "same account must not revive its binding"
            );
            tokio::task::yield_now().await;
            assert!(
                release.send(owner()).is_err(),
                "old I/O must have been cancelled"
            );
            while let Ok(command) = commands.try_recv() {
                assert!(matches!(command, Command::SignInEnded { .. }));
            }
            worker.request_jam(Request {
                expected: old,
                action: Action::Create(CreateIntent::Requested),
            });
            request(&mut worker, Action::Current);
            assert_eq!(
                fake.calls(),
                1,
                "restoration cannot reuse the retired transport"
            );
            assert_eq!(fake.writes(), 0);
        });
    }
}

#[test]
fn queued_owner_and_completed_current_cannot_revive_after_identity_restoration() {
    let _port = authorization_tests::WEB_PORT.lock().unwrap();
    for transition in [
        IdentityTransition::Browser,
        IdentityTransition::Verification,
        IdentityTransition::RemovePersonal,
    ] {
        for queued_result in [false, true] {
            // Each case owns its callback listener until its runtime stops.
            let (runtime, mut worker, fake, mut commands, events) =
                setup(&format!("jam-queued-loss-{transition:?}-{queued_result}"));
            prepare_identity(&mut worker, transition, false);
            let mut backend = consumer(&worker, events);
            runtime.block_on(async {
                fake.push(owner());
                request(&mut worker, Action::Current);
                let completed = if queued_result {
                    Some(queued(&mut commands).await)
                } else {
                    finish(&mut worker, &mut commands).await;
                    None
                };
                let old = backend.jam_stamp();
                // Leave either the owner event or the completion unread.
                transition.apply(&mut worker);
                restore_identity(&mut worker, transition);
                if let Some(completed) = completed {
                    worker.finish_jam(completed);
                }
                let event = backend
                    .poll_jam()
                    .expect("retirement must survive restoration");
                assert_eq!(event.phase, Phase::Invalidated(Cause::Account));
                assert!(event.snapshot.membership == Membership::Unknown);
                assert!(!worker.jam.gate.accepts(old));
                assert!(backend.poll_jam().is_none());
                assert_eq!(fake.calls(), 1);
            });
        }
    }
}

#[test]
fn another_ready_grant_keeps_same_account_binding_and_pending_current() {
    let _port = authorization_tests::WEB_PORT.lock().unwrap();
    for transition in [
        IdentityTransition::Browser,
        IdentityTransition::Verification,
        IdentityTransition::RemovePersonal,
    ] {
        let (runtime, mut worker, fake, mut commands, events) =
            setup(&format!("jam-retain-{transition:?}"));
        prepare_identity(&mut worker, transition, true);
        let mut backend = consumer(&worker, events);
        runtime.block_on(async {
            let release = fake.wait();
            request(&mut worker, Action::Current);
            tokio::task::yield_now().await;
            let old = backend.jam_stamp();
            backend.poll_jam();
            transition.apply(&mut worker);
            assert_eq!(worker.api.account(), Some(AccountId::new("alice")));
            assert!(worker.jam.gate.accepts(old));
            assert!(
                backend.poll_jam().is_none(),
                "no invalidation with another verified grant"
            );
            restore_identity(&mut worker, transition);
            release.send(owner()).ok().unwrap();
            finish(&mut worker, &mut commands).await;
            let event = backend.poll_jam().unwrap();
            assert_eq!(event.phase, Phase::Complete(Kind::Current));
            assert!(matches!(event.snapshot.membership, Membership::Present(_)));
            assert_eq!(fake.calls(), 1);
        });
    }
}

#[test]
fn context_invalidation_reaches_poll_jam_while_verification_is_unavailable() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-context-consumer");
    let mut backend = consumer(&worker, events);
    runtime.block_on(async {
        fake.push(owner());
        request(&mut worker, Action::Current);
        let completed = queued(&mut commands).await;
        worker.api.clear_all();
        assert!(
            backend.poll_jam().is_none(),
            "unverified pending event must be rejected"
        );
        worker.finish_jam(completed);
        let event = backend
            .poll_jam()
            .expect("context Unknown must reach the consumer");
        assert_eq!(event.phase, Phase::Invalidated(Cause::Context));
        assert!(event.snapshot.membership == Membership::Unknown);
        assert!(!worker.jam.gate.accepts(event.stamp));
    });
}

#[test]
fn attachment_and_notifications_are_passive_and_bounded() {
    let (runtime, mut worker, fake, _, events) = setup("jam-passive");
    let _entered = runtime.enter();
    assert_eq!(fake.calls(), 0);
    assert_eq!(fake.0.factories.load(Ordering::SeqCst), 0);
    assert!(latest(&events).snapshot.membership == Membership::Unknown);
    let gate = worker.jam.gate();
    let engine = gate.stamp().engine;
    for _ in 0..100 {
        gate.signal(engine, Notice::Jam);
    }
    assert_eq!(fake.calls(), 0);
    assert!(latest(&events).snapshot.membership == Membership::Unknown);
    assert!(latest(&events).snapshot.reconcile_required);
    assert!(latest(&events).phase == Phase::Invalidated(Cause::Jam));
    request(&mut worker, Action::Create(CreateIntent::Requested));
    assert_eq!(
        latest(&events).snapshot.error,
        Some(Error::ReconcileRequired)
    );
    assert_eq!(fake.0.factories.load(Ordering::SeqCst), 0);
}

#[test]
fn pending_then_verified_result_and_no_implicit_http() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-event-order");
    runtime.block_on(async {
        fake.push(absent());
        request(&mut worker, Action::Current);
        let pending = latest(&events);
        assert!(pending.phase == Phase::Pending(Kind::Current));
        finish(&mut worker, &mut commands).await;
        let absent = latest(&events);
        assert!(absent.phase == Phase::Complete(Kind::Current));
        assert!(absent.snapshot.membership == Membership::Absent);
        assert!(pending.stamp == absent.stamp);
        fake.push(owner()); fake.push(owner());
        request(&mut worker, Action::Create(CreateIntent::Requested));
        assert!(latest(&events).phase == Phase::Pending(Kind::Create));
        finish(&mut worker, &mut commands).await;
        assert!(matches!(latest(&events).snapshot.membership, Membership::Present(ref p) if p.is_session_owner));
        assert_eq!(fake.writes(), 1);
        assert_eq!(fake.calls(), 3);
        tokio::task::yield_now().await;
        assert_eq!(fake.calls(), 3, "no background refresh");
    });
}

#[test]
fn budget_wait_is_cancelled_on_logout_and_does_not_take_a_slot_from_the_next_account() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-budget-logout");
    runtime.block_on(async {
        let held = Arc::clone(&worker.background_session)
            .acquire_many_owned(4)
            .await
            .unwrap();
        request(&mut worker, Action::Current);
        tokio::task::yield_now().await;
        assert_eq!(fake.calls(), 0);
        let old = latest(&events);
        worker.sign_out();
        drop(held);
        tokio::task::yield_now().await;
        assert!(!worker.jam.gate.accepts(old.stamp));
        assert_eq!(fake.calls(), 0);
        assert!(commands.try_recv().is_err());
        worker
            .api
            .install(ApiSource::Shared, AccountId::new("bob"))
            .unwrap();
        worker.signed_in = true;
        let next = Fake::new("bob");
        bind(&mut worker, &next, "bob");
        next.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        assert_eq!(next.calls(), 1);
        assert_eq!(worker.background_session.available_permits(), 4);
        assert!(latest(&events).stamp.account != old.stamp.account);
    });
}

#[test]
fn success_verification_reads_share_the_existing_background_budget() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-verify-budget");
    runtime.block_on(async {
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let held = Arc::clone(&worker.background_session)
            .acquire_many_owned(4)
            .await
            .unwrap();
        fake.push(owner());
        fake.push(owner());
        request(&mut worker, Action::Create(CreateIntent::Requested));
        tokio::task::yield_now().await;
        assert_eq!(
            fake.calls(),
            2,
            "mutation sent; verification still waiting for read budget"
        );
        assert!(latest(&events).phase == Phase::Pending(Kind::Create));
        drop(held);
        finish(&mut worker, &mut commands).await;
        assert_eq!(fake.calls(), 3);
        assert_eq!(fake.writes(), 1);
        assert_eq!(worker.background_session.available_permits(), 4);
    });
}

#[test]
fn concurrent_intentions_do_not_retire_the_running_operation_or_emit_a_second_write() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-serial");
    runtime.block_on(async {
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let release = fake.wait();
        fake.push(owner());
        request(&mut worker, Action::Create(CreateIntent::Requested));
        tokio::task::yield_now().await;
        let stamp = latest(&events).stamp;
        request(&mut worker, Action::Create(CreateIntent::Requested));
        assert_eq!(latest(&events).snapshot.error, Some(Error::Busy));
        assert!(latest(&events).stamp == stamp);
        assert!(latest(&events).phase == Phase::Pending(Kind::Create));
        release.send(owner()).ok().unwrap();
        finish(&mut worker, &mut commands).await;
        assert!(latest(&events).phase == Phase::Complete(Kind::Create));
        assert_eq!(latest(&events).snapshot.error, None);
        assert_eq!(fake.writes(), 1);
    });
}

#[test]
fn queued_current_create_and_end_results_cannot_apply_after_retirement() {
    for kind in [Kind::Current, Kind::Create, Kind::End] {
        let (runtime, mut worker, fake, mut commands, events) =
            setup(&format!("jam-late-{kind:?}"));
        runtime.block_on(async {
            if kind != Kind::Current {
                fake.push(if kind == Kind::Create {
                    absent()
                } else {
                    owner()
                });
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
            }
            let action = match kind {
                Kind::Current => {
                    fake.push(owner());
                    Action::Current
                }
                Kind::Create => {
                    fake.push(owner());
                    fake.push(owner());
                    Action::Create(CreateIntent::Requested)
                }
                Kind::End => {
                    fake.push(empty(204));
                    fake.push(absent());
                    Action::End {
                        session_id: "synthetic-jam".into(),
                        intent: EndIntent::ForEveryone,
                    }
                }
            };
            request(&mut worker, action);
            let completed = queued(&mut commands).await;
            let retired = completed.stamp;
            worker.sign_out();
            worker
                .api
                .install(ApiSource::Shared, AccountId::new("bob"))
                .unwrap();
            worker.signed_in = true;
            let next = Fake::new("bob");
            bind(&mut worker, &next, "bob");
            let current = latest(&events).stamp;
            worker.finish_jam(completed);
            assert!(latest(&events).stamp == current);
            assert!(latest(&events).snapshot.membership == Membership::Unknown);
            assert!(!worker.jam.gate.accepts(retired));
            assert_eq!(next.calls(), 0);
        });
    }
}

#[test]
fn dealer_jam_signals_cancel_inflight_io_and_never_establish_absence() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-signal-cancel");
    runtime.block_on(async {
        let release = fake.wait();
        request(&mut worker, Action::Current);
        tokio::task::yield_now().await;
        let old = latest(&events).stamp;
        let gate = worker.jam.gate();
        gate.signal(old.engine, Notice::Jam);
        tokio::task::yield_now().await;
        assert!(
            release.send(owner()).is_err(),
            "retired read was actually cancelled"
        );
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert!(!gate.accepts(old));
        assert!(commands.try_recv().is_err());
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        assert!(latest(&events).snapshot.membership == Membership::Absent);
        assert_eq!(fake.calls(), 2);
    });
}

#[test]
fn engine_replace_and_connection_changes_fence_the_same_account() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-engine-connection");
    runtime.block_on(async {
        fake.push(owner());
        request(&mut worker, Action::Current);
        let late = queued(&mut commands).await;
        let old = late.stamp;
        worker.take_engine_for_resume();
        bind(&mut worker, &fake, "alice");
        worker.finish_jam(late);
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert!(latest(&events).stamp.engine != old.engine);
        let gate = worker.jam.gate();
        let engine = gate.stamp().engine;
        gate.signal(engine, Notice::Connection(Some("connection-b".into())));
        let after_signal = gate.stamp();
        request(&mut worker, Action::Current);
        assert_eq!(latest(&events).snapshot.error, Some(Error::NotReady));
        assert_eq!(
            fake.calls(),
            1,
            "Spirc has not installed the new connection yet"
        );
        fake.0.context.lock().unwrap().connection = "connection-b".into();
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        assert!(latest(&events).snapshot.membership == Membership::Absent);
        worker.request_jam(Request {
            expected: after_signal,
            action: Action::Create(CreateIntent::Requested),
        });
        assert_eq!(latest(&events).snapshot.error, Some(Error::Stale));
        assert_eq!(fake.writes(), 0);
    });
}

#[test]
fn listener_close_requires_an_engine_boundary_instead_of_subscription_repair() {
    let (runtime, mut worker, fake, _, events) = setup("jam-listener-close");
    let _entered = runtime.enter();
    let gate = worker.jam.gate();
    let engine = gate.stamp().engine;
    gate.signal(engine, Notice::Closed);
    let closed = gate.stamp();
    gate.signal(engine, Notice::Jam);
    assert!(gate.stamp() == closed);
    request(&mut worker, Action::Current);
    assert_eq!(latest(&events).snapshot.error, Some(Error::NotReady));
    assert_eq!(fake.calls(), 0);
    assert_eq!(fake.0.factories.load(Ordering::SeqCst), 0);
}

#[test]
fn expired_or_rejected_writes_remain_unknown_without_refresh_or_retry() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-expired-rejected");
    runtime.block_on(async {
        tokio::time::pause();
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        tokio::time::advance(Duration::from_secs(15)).await;
        request(&mut worker, Action::Create(CreateIntent::Requested));
        finish(&mut worker, &mut commands).await;
        assert_eq!(latest(&events).snapshot.error, Some(Error::SnapshotExpired));
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!(fake.writes(), 0);
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        fake.push(empty(405));
        request(&mut worker, Action::Create(CreateIntent::Requested));
        finish(&mut worker, &mut commands).await;
        assert_eq!(latest(&events).snapshot.error, Some(Error::Http(405)));
        assert_eq!(fake.calls(), 3);
        request(&mut worker, Action::Create(CreateIntent::Requested));
        finish(&mut worker, &mut commands).await;
        assert_eq!(
            latest(&events).snapshot.error,
            Some(Error::ReconcileRequired)
        );
        assert_eq!(fake.calls(), 3);
    });
}

#[test]
fn verification_account_loss_and_unannounced_context_change_block_delivery() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-account-gate");
    runtime.block_on(async {
        fake.push(owner());
        request(&mut worker, Action::Current);
        let completed = queued(&mut commands).await;
        worker.api.clear_all();
        assert!(!worker.jam.gate.accepts(completed.stamp));
        worker.finish_jam(completed);
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        worker
            .api
            .install(ApiSource::Shared, AccountId::new("alice"))
            .unwrap();
        fake.push(owner());
        request(&mut worker, Action::Current);
        let completed = queued(&mut commands).await;
        fake.0.context.lock().unwrap().device = "replacement-device".into();
        assert!(!worker.jam.gate.accepts(completed.stamp));
        worker.finish_jam(completed);
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!(fake.writes(), 0);
    });
}

#[test]
fn backend_loop_routes_commands_and_fences_completions_through_the_proxy_barrier() {
    let (runtime, mut worker, fake, commands, mut events) = setup("jam-real-dispatch");
    let sender = worker.commands.clone();
    let expected = worker.jam.gate.stamp();
    fake.push(absent());
    sender
        .send(Command::Jam(Request {
            expected,
            action: Action::Current,
        }))
        .ok()
        .unwrap();
    runtime.block_on(async {
        let observe = async {
            loop {
                events.changed().await.unwrap();
                if latest(&events).phase == Phase::Complete(Kind::Current) {
                    break;
                }
            }
            sender.send(Command::Shutdown).ok().unwrap();
        };
        tokio::join!(worker.run(commands), observe);
    });
    assert_eq!(fake.calls(), 1);

    let (runtime, mut worker, fake, commands, mut events) = setup("jam-proxy-barrier-result");
    runtime.block_on(async {
        let release = fake.wait();
        request(&mut worker, Action::Current);
        tokio::task::yield_now().await;
        worker.restoring_proxy = true;
        let sender = worker.commands.clone();
        let observe = async {
            release.send(absent()).ok().unwrap();
            loop {
                events.changed().await.unwrap();
                if latest(&events).phase == Phase::Complete(Kind::Current) {
                    break;
                }
            }
            sender.send(Command::Shutdown).ok().unwrap();
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(worker.run(commands), observe);
        })
        .await
        .unwrap();
    });
    assert_eq!(fake.calls(), 1);
}

#[test]
fn in_flight_create_and_end_are_cancelled_without_verification_or_replay() {
    for kind in [Kind::Create, Kind::End] {
        let (runtime, mut worker, fake, mut commands, events) =
            setup(&format!("jam-cancel-{kind:?}"));
        runtime.block_on(async {
            fake.push(if kind == Kind::Create {
                absent()
            } else {
                owner()
            });
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let release = fake.wait();
            request(
                &mut worker,
                match kind {
                    Kind::Create => Action::Create(CreateIntent::Requested),
                    _ => Action::End {
                        session_id: "synthetic-jam".into(),
                        intent: EndIntent::ForEveryone,
                    },
                },
            );
            tokio::task::yield_now().await;
            assert_eq!(fake.writes(), 1);
            worker.take_engine_for_resume();
            tokio::task::yield_now().await;
            assert!(release.send(empty(204)).is_err());
            assert!(commands.try_recv().is_err());
            assert_eq!(fake.calls(), 2);
            assert!(latest(&events).snapshot.membership == Membership::Unknown);
        });
    }
}

#[test]
fn initial_or_unannounced_connection_installation_needs_only_a_deliberate_read() {
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-connection-install");
    runtime.block_on(async {
        worker.jam.gate.lock().connection = None;
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        fake.0.context.lock().unwrap().connection = "connection-b".into();
        request(&mut worker, Action::Current);
        assert_eq!(latest(&events).snapshot.error, Some(Error::Stale));
        assert_eq!(fake.calls(), 1);
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let gate = worker.jam.gate();
        let engine = gate.stamp().engine;
        gate.signal(engine, Notice::Jam);
        fake.0.context.lock().unwrap().connection = "connection-c".into();
        request(&mut worker, Action::Current);
        assert_eq!(latest(&events).snapshot.error, Some(Error::Stale));
        assert_eq!(fake.calls(), 2);
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        gate.signal(engine, Notice::Connection(None));
        request(&mut worker, Action::Current);
        assert_eq!(latest(&events).snapshot.error, Some(Error::NotReady));
        assert_eq!(
            fake.calls(),
            3,
            "a missing announced header is not guessed from a getter"
        );
    });
}
