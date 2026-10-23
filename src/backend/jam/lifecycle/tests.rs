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
    generation_prepares: AtomicUsize,
    generation_posts: AtomicUsize,
    generation_auth_fail: AtomicBool,
    generation_status: AtomicUsize,
    generation_auth: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    generation_reply: Mutex<Option<tokio::sync::oneshot::Receiver<Reply>>>,
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
            generation_prepares: AtomicUsize::new(0),
            generation_posts: AtomicUsize::new(0),
            generation_auth_fail: AtomicBool::new(false),
            generation_status: AtomicUsize::new(200),
            generation_auth: Mutex::new(None),
            generation_reply: Mutex::new(None),
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
    fn generate<'a>(
        &'a self,
        _uri: &'a generation::JoinUri,
        _context: &'a Context,
        lease: &'a generation::Lease,
        check: generation::Permit<'a>,
    ) -> generation::Response<'a> {
        Box::pin(async move {
            check()?;
            self.0.generation_prepares.fetch_add(1, Ordering::SeqCst);
            let wait = self.0.generation_auth.lock().unwrap().take();
            if let Some(wait) = wait {
                wait.await
                    .map_err(|_| generation::Terminal::AuthUnavailable)?;
            }
            check()?;
            if self.0.generation_auth_fail.load(Ordering::SeqCst) {
                return Err(generation::Terminal::AuthUnavailable);
            }
            lease.dispatch()?;
            self.0.generation_posts.fetch_add(1, Ordering::SeqCst);
            let wait = self.0.generation_reply.lock().unwrap().take();
            let reply = if let Some(wait) = wait {
                wait.await.map_err(|_| generation::Terminal::Transport)?
            } else {
                generation_reply(self.0.generation_status.load(Ordering::SeqCst) as u16)
            };
            check()?;
            Ok(reply)
        })
    }
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
            dispatch.mark_emitted();
            if let Some(probe) = &dispatch.diagnostic {
                probe.stage(Stage::Transport);
            }
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

fn generation_owner() -> Reply {
    Reply { status:200, content_type:None, body:br#"{"session_id":"synthetic-jam","is_session_owner":true,"is_listening":false,"is_controlling":true,"session_owner_id":"alice","join_session_uri":"spotify://sessions/join/SyntheticJam08B"}"#.to_vec() }
}
fn generation_reply(status: u16) -> Reply {
    Reply {
        status,
        content_type: Some(b"application/json".to_vec()),
        body: br#"{"shareable_url":"https://spotify.link/SyntheticInvite08B"}"#.to_vec(),
    }
}
async fn generation_complete(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
) -> GenerationCompleted {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await.unwrap() {
                Command::JamGenerationFinished(result) => return *result,
                Command::SignInEnded { .. } => {}
                _ => panic!("unexpected completion"),
            }
        }
    })
    .await
    .unwrap()
}
async fn generation_started(fake: &Fake, dispatched: bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while if dispatched {
            fake.0.generation_posts.load(Ordering::SeqCst)
        } else {
            fake.0.generation_prepares.load(Ordering::SeqCst)
        } == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn generation_real_bridge_keeps_membership_and_total_cap_across_retirement() {
    for status in [200, 201, 301, 401, 429, 503] {
        let (runtime, mut worker, fake, mut commands, events) = setup("generation-total-cap");
        runtime.block_on(async {
            fake.push(generation_owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            fake.0.generation_status.store(status, Ordering::SeqCst);
            worker.generate_jam(generation::Intent::explicitly_authorized(
                worker.jam.gate.stamp(),
            ));
            let complete = generation_complete(&mut commands).await;
            worker.finish_jam_generation(complete);
            let expected = if status == 200 || status == 201 {
                generation::Terminal::Accepted(generation::Family::Short)
            } else {
                generation::Terminal::Http(status as u16)
            };
            assert_eq!(worker.jam.gate.generation_report(), Some(expected));
            assert!(matches!(
                latest(&events).snapshot.membership,
                Membership::Present(_)
            ));
            assert!(!latest(&events).snapshot.reconcile_required);
            assert!(latest(&events).snapshot.metadata.invitation.is_none());
            let stamp = worker.jam.gate.stamp();
            worker.jam.gate.signal(stamp.engine, Notice::Jam);
            // Read again explicitly; no experiment refresh is emitted.
            fake.push(generation_owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            worker.generate_jam(generation::Intent::explicitly_authorized(
                worker.jam.gate.stamp(),
            ));
            assert_eq!(
                worker.jam.gate.generation_report(),
                Some(generation::Terminal::Exhausted)
            );
            assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 1);
            assert_eq!(fake.calls(), 2);
            bind(&mut worker, &fake, "alice"); // Engine retirement/bind also retains allowance.
            fake.push(generation_owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            worker.generate_jam(generation::Intent::explicitly_authorized(
                worker.jam.gate.stamp(),
            ));
            assert_eq!(
                worker.jam.gate.generation_report(),
                Some(generation::Terminal::Exhausted)
            );
            assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 1);
            assert_eq!(fake.calls(), 3);
        });
    }
}

#[test]
fn generation_bridge_refuses_actual_registration_stamp_owner_uri_uncertainty_and_auth_guards() {
    for guard in 0..11 {
        let (runtime, mut worker, fake, mut commands, _events) = setup("generation-guards");
        runtime.block_on(async {
            let mut current = generation_owner();
            if guard == 3 || guard == 10 {
                let mut root: serde_json::Value = serde_json::from_slice(&current.body).unwrap();
                root.as_object_mut().unwrap().remove(if guard == 3 {
                    "join_session_uri"
                } else {
                    "session_owner_id"
                });
                if guard == 3 {
                    root["join_session_url"] = "hm:synthetic".into();
                    root["join_session_token"] = "Synthetic".into();
                }
                current.body = serde_json::to_vec(&root).unwrap();
            }
            if guard == 4 {
                let mut root: serde_json::Value = serde_json::from_slice(&current.body).unwrap();
                root["is_session_owner"] = false.into();
                current.body = serde_json::to_vec(&root).unwrap();
            }
            fake.push(current);
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let mut intent = generation::Intent::explicitly_authorized(worker.jam.gate.stamp());
            match guard {
                0 => intent.expected.operation += 1,
                1 => worker.jam.gate.lock().listening = false,
                2 => worker
                    .jam
                    .gate
                    .lock()
                    .write_uncertain
                    .store(true, Ordering::SeqCst),
                3 | 4 | 10 => {}
                5 => worker.jam.gate.lock().verify = Arc::new(|| false),
                6 => fake.0.context.lock().unwrap().connection = "replacement".into(),
                7 => intent.deadline = Instant::now(),
                8 => fake.0.generation_auth_fail.store(true, Ordering::SeqCst),
                _ => worker.jam.gate.lock().connection = None,
            }
            worker.generate_jam(intent);
            if guard == 8 {
                worker.finish_jam_generation(generation_complete(&mut commands).await);
                assert_eq!(
                    worker.jam.gate.generation_report(),
                    Some(generation::Terminal::AuthUnavailable)
                );
            }
            assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 0);
            assert_eq!(fake.calls(), 1);
        });
    }
}

#[test]
fn generation_bridge_retires_during_auth_response_and_queued_delivery_without_late_http() {
    for stage in 0..3 {
        for cause in 0..4 {
            let (runtime, mut worker, fake, mut commands, _events) = setup("generation-retire");
            runtime.block_on(async {
                fake.push(generation_owner());
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
                let (auth_tx, auth_rx) = tokio::sync::oneshot::channel();
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if stage == 0 {
                    *fake.0.generation_auth.lock().unwrap() = Some(auth_rx);
                }
                if stage == 1 {
                    *fake.0.generation_reply.lock().unwrap() = Some(reply_rx);
                }
                worker.generate_jam(generation::Intent::explicitly_authorized(
                    worker.jam.gate.stamp(),
                ));
                let queued = if stage == 2 {
                    Some(generation_complete(&mut commands).await)
                } else {
                    generation_started(&fake, stage == 1).await;
                    None
                };
                let gate = worker.jam.gate();
                let stamp = gate.stamp();
                match cause {
                    0 => gate.signal(stamp.engine, Notice::Jam),
                    1 => gate.signal(stamp.engine, Notice::Connection(Some("replacement".into()))),
                    2 => gate.signal(stamp.engine, Notice::Closed),
                    _ => gate.cancel_generation(),
                }
                let _ = auth_tx.send(());
                let _ = reply_tx.send(generation_reply(200));
                if let Some(queued) = queued {
                    worker.finish_jam_generation(queued);
                }
                tokio::task::yield_now().await;
                assert!(matches!(
                    gate.generation_report(),
                    Some(generation::Terminal::Invalidated | generation::Terminal::Cancelled)
                ));
                assert_eq!(
                    fake.0.generation_posts.load(Ordering::SeqCst),
                    usize::from(stage != 0)
                );
                assert_eq!(fake.calls(), 1);
            });
        }
    }
}

#[test]
fn generation_budget_wait_expires_original_freshness_and_busy_cannot_dispatch_twice() {
    let (runtime, mut worker, fake, mut commands, _events) = setup("generation-budget");
    runtime.block_on(async {
        tokio::time::pause();
        fake.push(generation_owner());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let slots = worker.background_session.available_permits();
        let budget = Arc::clone(&worker.background_session);
        let occupied = budget.acquire_many(slots as u32).await.unwrap();
        worker.generate_jam(generation::Intent::explicitly_authorized(
            worker.jam.gate.stamp(),
        ));
        worker.generate_jam(generation::Intent::explicitly_authorized(
            worker.jam.gate.stamp(),
        ));
        assert_eq!(
            worker.jam.gate.generation_report(),
            Some(generation::Terminal::Busy)
        );
        tokio::task::yield_now().await;
        tokio::time::advance(FRESH_FOR).await;
        drop(occupied);
        worker.finish_jam_generation(generation_complete(&mut commands).await);
        assert_eq!(
            worker.jam.gate.generation_report(),
            Some(generation::Terminal::Invalidated)
        );
        assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 0);
        assert_eq!(fake.calls(), 1);
    });
}

#[test]
fn generation_cancelled_in_backend_queue_cannot_dispatch_or_replace_newer_intent() {
    let (runtime, mut worker, fake, mut commands, _events) = setup("generation-queued-cancel");
    runtime.block_on(async {
        fake.push(generation_owner());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let gate = worker.jam.gate();
        let old = generation::Intent::explicitly_authorized(gate.stamp());
        assert!(gate.arm_generation(&old));
        assert!(!gate.arm_generation(&generation::Intent::explicitly_authorized(gate.stamp())));
        gate.cancel_generation();
        let new = generation::Intent::explicitly_authorized(gate.stamp());
        assert!(gate.arm_generation(&new));
        worker.generate_jam(old);
        assert!(
            gate.lock()
                .generation_queued
                .as_ref()
                .is_some_and(|(slot, _)| Arc::ptr_eq(slot, &new.cancelled))
        );
        gate.cancel_generation();
        worker.generate_jam(new);
        assert_eq!(
            gate.generation_report(),
            Some(generation::Terminal::Cancelled)
        );
        assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 0);
        assert_eq!(fake.0.generation_prepares.load(Ordering::SeqCst), 0);
        tokio::time::pause();
        let expired = generation::Intent::explicitly_authorized(gate.stamp());
        assert!(gate.arm_generation(&expired));
        tokio::time::advance(DEADLINE).await;
        assert_eq!(
            gate.generation_report(),
            Some(generation::Terminal::Deadline)
        );
        worker.generate_jam(expired);
        assert_eq!(
            gate.generation_report(),
            Some(generation::Terminal::Deadline)
        );
        assert_eq!(fake.0.generation_posts.load(Ordering::SeqCst), 0);
    });
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
        Arc::clone(&worker.jam.gate.lock().write_uncertain),
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

#[test]
fn private_local_emission_rechecks_context_even_without_a_new_stamp_or_event() {
    let (_runtime, mut worker, fake, _commands, _events) = setup("jam-private-emission");
    let gate = worker.jam.gate();
    let stamp = gate.stamp();
    let emitted = AtomicUsize::new(0);
    assert!(
        gate.with_context(stamp, || {
            assert!(
                gate.0.try_lock().is_err(),
                "apply must retain the context gate"
            );
            emitted.fetch_add(1, Ordering::SeqCst)
        })
        .is_some()
    );
    let stale = Stamp {
        operation: stamp.operation.wrapping_add(1),
        ..stamp
    };
    assert!(
        gate.with_context(stale, || emitted.fetch_add(1, Ordering::SeqCst))
            .is_none()
    );
    // Account verification can fail before its notification reaches the bridge.
    gate.lock().verify = Arc::new(|| false);
    assert_eq!(gate.stamp(), stamp);
    assert!(
        gate.with_context(stamp, || emitted.fetch_add(1, Ordering::SeqCst))
            .is_none()
    );
    assert_eq!(emitted.load(Ordering::SeqCst), 1);
    assert_eq!(fake.calls(), 0);
    worker.jam.retire(stamp.account, Cause::Account);
}

fn request(worker: &mut Worker, action: Action) {
    let expected = worker.jam.gate.stamp();
    worker.request_jam(Request { expected, action });
}

fn diagnose(worker: &mut Worker) -> Probe {
    let probe = Probe::default();
    let action = Action::DiagnoseCurrent(probe.clone());
    worker.jam.gate.arm_diagnostic(&action);
    request(worker, action);
    probe
}

#[test]
fn create_observation_survives_jam_until_one_live_verifier() {
    for notices in 0..4 {
        let (runtime, mut worker, fake, mut commands, events) = setup("spike-create-positive");
        runtime.block_on(async {
            fake.push(absent());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let release = fake.wait();
            let mut current = owner();
            let mut body: serde_json::Value = serde_json::from_slice(&current.body).unwrap();
            // Same deliberately rejected invitation as the baseline characterization.
            body["join_session_url"] = serde_json::json!(
                "https://open.spotify.com/socialsession/SyntheticPrivate?unknown=x"
            );
            current.body = serde_json::to_vec(&body).unwrap();
            fake.push(current);
            request(&mut worker, Action::Create(CreateIntent::Requested));
            tokio::time::timeout(Duration::from_secs(2), async {
                while fake.calls() < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let gate = worker.jam.gate();
            let old = gate.stamp();
            let lease = Arc::clone(gate.lock().pending_create.as_ref().unwrap());
            let deadline = lease.deadline;
            for _ in 0..notices {
                gate.signal(old.engine, Notice::Jam);
                assert!(latest(&events).phase == Phase::Invalidated(Cause::Jam));
                assert!(latest(&events).snapshot.membership == Membership::Unknown);
                assert!(latest(&events).snapshot.metadata.invitation.is_none());
                assert!(!gate.accepts(old));
                assert_eq!(lease.deadline, deadline);
            }
            assert!(!release.is_closed());
            assert!(gate.write_uncertain());
            assert_eq!(fake.calls(), 2, "no read before valid reply");
            let mut created = owner();
            let mut body: serde_json::Value = serde_json::from_slice(&created.body).unwrap();
            body["join_session_url"] =
                serde_json::json!("https://open.spotify.com/socialsession/SyntheticPrivate");
            created.body = serde_json::to_vec(&body).unwrap();
            release.send(created).ok().unwrap();
            let completed = queued(&mut commands).await;
            assert!(
                gate.write_uncertain(),
                "ack and read alone do not publish authority"
            );
            assert!(latest(&events).snapshot.membership == Membership::Unknown);
            worker.finish_jam(completed);
            assert!(!gate.write_uncertain());
            assert_eq!(latest(&events).snapshot.error, None);
            assert!(!latest(&events).snapshot.reconcile_required);
            let mut model = crate::model::jam::State::default();
            assert!(model.observe(&latest(&events), gate.stamp()));
            assert!(model.fresh() && model.owner_id().is_some());
            assert!(
                model.invitation().is_none(),
                "never retain invitation from Create"
            );
            assert_eq!((fake.calls(), fake.writes()), (3, 1));
        });
    }
}

#[test]
fn create_hard_retire_errors_deadlines_and_late_results_remain_blocked() {
    // 0..4: hard retire; 5..9: missing/invalid reply or discordant verifier;
    // 10: Jam in verifier; 11..12: original deadline; 13: late Jam delivery;
    // 14: getter changed; 15: Jam then hard retire; 16..17: delivery expiry.
    for case in 0..18 {
        let (runtime, mut worker, fake, mut commands, events) = setup("spike-create-negative");
        runtime.block_on(async {
            if matches!(case, 11 | 12 | 16 | 17) {
                tokio::time::pause();
            }
            fake.push(absent());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let mut release = fake.wait();
            let mut verifier = if matches!(case, 10 | 12) {
                Some(fake.wait())
            } else {
                if !matches!(case, 5 | 6 | 7 | 11 | 14) {
                    let mut reply = if case == 8 { absent() } else { owner() };
                    if case == 9 {
                        let mut body: serde_json::Value =
                            serde_json::from_slice(&reply.body).unwrap();
                        body["session_id"] = serde_json::json!("synthetic-other");
                        reply.body = serde_json::to_vec(&body).unwrap();
                    }
                    fake.push(reply);
                }
                None
            };
            request(&mut worker, Action::Create(CreateIntent::Requested));
            tokio::time::timeout(Duration::from_secs(2), async {
                while fake.calls() < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let gate = worker.jam.gate();
            let old = gate.stamp();
            if case < 5 || case == 15 {
                match case {
                    0 => worker.jam.retire(old.account + 1, Cause::Account),
                    1 => worker.take_engine_for_resume(),
                    2 => gate.signal(
                        old.engine,
                        Notice::Connection(Some("other-connection".into())),
                    ),
                    3 => gate.lock().invalidate(Cause::Context),
                    4 => gate.signal(old.engine, Notice::Closed),
                    _ => {
                        gate.signal(old.engine, Notice::Jam);
                        gate.signal(old.engine, Notice::Jam);
                        gate.lock().invalidate(Cause::Context);
                    }
                }
                tokio::time::timeout(Duration::from_secs(2), release.closed())
                    .await
                    .unwrap();
                assert!(release.send(owner()).is_err());
                assert!(commands.try_recv().is_err());
            } else if case == 11 {
                gate.signal(old.engine, Notice::Jam);
                tokio::time::advance(Duration::from_secs(21)).await;
                finish(&mut worker, &mut commands).await;
                assert!(release.send(owner()).is_err());
                assert_eq!(latest(&events).snapshot.error, Some(Error::Timeout));
            } else {
                if case == 14 {
                    fake.0.context.lock().unwrap().connection = "changed-without-notice".into();
                }
                if case == 12 {
                    tokio::time::advance(Duration::from_secs(19)).await;
                }
                if case == 5 {
                    drop(release);
                } else {
                    let mut reply = if case == 6 { empty(200) } else { owner() };
                    if case == 7 {
                        let mut body: serde_json::Value =
                            serde_json::from_slice(&reply.body).unwrap();
                        body["is_session_owner"] = serde_json::json!(false);
                        reply.body = serde_json::to_vec(&body).unwrap();
                    }
                    release.send(reply).ok().unwrap();
                }
                if matches!(case, 10 | 12) {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while fake.calls() < 3 {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    let mut sender = verifier.take().unwrap();
                    if case == 10 {
                        gate.signal(old.engine, Notice::Jam);
                        gate.signal(old.engine, Notice::Jam);
                        tokio::time::timeout(Duration::from_secs(2), sender.closed())
                            .await
                            .unwrap();
                        assert!(commands.try_recv().is_err());
                    } else {
                        tokio::time::advance(Duration::from_secs(2)).await;
                        finish(&mut worker, &mut commands).await;
                        assert_eq!(latest(&events).snapshot.error, Some(Error::Timeout));
                    }
                    assert!(sender.send(owner()).is_err());
                } else if matches!(case, 13 | 16 | 17) {
                    let completed = queued(&mut commands).await;
                    assert!(gate.write_uncertain());
                    if case == 13 {
                        gate.signal(old.engine, Notice::Jam);
                    } else {
                        tokio::time::advance(Duration::from_secs(if case == 16 { 21 } else { 16 }))
                            .await;
                    }
                    worker.finish_jam(completed);
                    assert!(latest(&events).snapshot.membership == Membership::Unknown);
                } else {
                    finish(&mut worker, &mut commands).await;
                }
            }
            assert!(gate.write_uncertain(), "case {case}");
            assert!(latest(&events).snapshot.membership == Membership::Unknown);
            assert!(latest(&events).snapshot.metadata.invitation.is_none());
            assert!(latest(&events).snapshot.reconcile_required);
            assert_eq!(fake.writes(), 1);
            assert!(fake.calls() <= 3, "no verification retry");
        });
    }
}

#[test]
fn jam_before_create_emission_does_not_preserve_a_permission() {
    let (runtime, mut worker, fake, mut commands, events) = setup("spike-create-before-emission");
    runtime.block_on(async {
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        request(&mut worker, Action::Create(CreateIntent::Requested));
        let gate = worker.jam.gate();
        gate.signal(gate.stamp().engine, Notice::Jam);
        tokio::task::yield_now().await;
        assert_eq!((fake.calls(), fake.writes()), (1, 0));
        assert!(!gate.write_uncertain());
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert!(commands.try_recv().is_err());
    });
}

#[test]
fn failed_witness_or_verifier_and_ordinary_current_cannot_clear_uncertainty() {
    for case in 0..10 {
        let (runtime, mut worker, fake, mut commands, events) = setup("spike-create-failed-proof");
        runtime.block_on(async {
            fake.push(absent());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let release = fake.wait();
            if case >= 6 {
                let mut reply = if case == 6 { absent() } else { owner() };
                if matches!(case, 7 | 8) {
                    let mut body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
                    if case == 7 {
                        body["session_id"] = serde_json::json!("synthetic-other");
                    } else {
                        body["is_session_owner"] = serde_json::json!(false);
                    }
                    reply.body = serde_json::to_vec(&body).unwrap();
                }
                if case == 9 {
                    drop(fake.wait());
                } else {
                    fake.push(reply);
                }
            }
            request(&mut worker, Action::Create(CreateIntent::Requested));
            tokio::time::timeout(Duration::from_secs(2), async {
                while fake.calls() < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let gate = worker.jam.gate();
            if case != 0 {
                gate.signal(gate.stamp().engine, Notice::Jam);
                gate.signal(gate.stamp().engine, Notice::Jam);
            }
            if case == 0 {
                drop(release);
            } else {
                let mut reply = match case {
                    1 => empty(200),
                    4 => empty(500),
                    5 => empty(403),
                    _ => owner(),
                };
                if matches!(case, 2 | 3) {
                    let mut body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
                    if case == 2 {
                        body["is_session_owner"] = serde_json::json!(false);
                    } else {
                        body["session_id"] = serde_json::json!("");
                    }
                    reply.body = serde_json::to_vec(&body).unwrap();
                }
                release.send(reply).ok().unwrap();
            }
            let completed = queued(&mut commands).await;
            if case == 0 {
                // Failure is already queued: a Jam must not leave a finished task Busy.
                gate.signal(gate.stamp().engine, Notice::Jam);
            }
            worker.finish_jam(completed);
            assert!(gate.lock().pending.is_none(), "case {case}");
            assert!(gate.lock().pending_create.is_none(), "case {case}");
            assert!(gate.write_uncertain());
            assert!(latest(&events).snapshot.membership == Membership::Unknown);
            assert_eq!(fake.calls(), if case < 6 { 2 } else { 3 });
            for reply in [absent(), owner()] {
                fake.push(reply);
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
                assert!(gate.write_uncertain());
                assert_eq!(latest(&events).snapshot.error, Some(Error::WriteUncertain));
                let mut model = crate::model::jam::State::default();
                assert!(model.observe(&latest(&events), gate.stamp()));
                assert!(!model.fresh() && model.owner_id().is_none());
            }
            let calls = fake.calls();
            request(&mut worker, Action::Create(CreateIntent::Requested));
            finish(&mut worker, &mut commands).await;
            assert_eq!(latest(&events).snapshot.error, Some(Error::WriteUncertain));
            assert_eq!(
                fake.calls(),
                calls,
                "a new intention cannot replay uncertain Create"
            );
            assert_eq!(fake.writes(), 1);
        });
    }
}

#[test]
fn jam_while_verifier_waits_for_budget_does_not_rebind_or_emit_a_get() {
    let (runtime, mut worker, fake, mut commands, events) = setup("spike-create-budget-retired");
    runtime.block_on(async {
        fake.push(absent());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let held = Arc::clone(&worker.background_session)
            .acquire_many_owned(4)
            .await
            .unwrap();
        fake.push(owner());
        request(&mut worker, Action::Create(CreateIntent::Requested));
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.calls() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let gate = worker.jam.gate();
        let lease = Arc::clone(gate.lock().pending_create.as_ref().unwrap());
        assert!(lease.emitted.load(Ordering::SeqCst));
        assert!(!lease.verifier_emitted.load(Ordering::SeqCst));
        let deadline = lease.deadline;
        gate.signal(gate.stamp().engine, Notice::Jam);
        gate.signal(gate.stamp().engine, Notice::Jam);
        drop(held);
        finish(&mut worker, &mut commands).await;
        assert!(gate.write_uncertain());
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!((fake.calls(), fake.writes()), (2, 1));
        assert_eq!(lease.deadline, deadline);
        assert_eq!(worker.background_session.available_permits(), 4);
    });
}

#[test]
fn generic_host_cannot_dispatch_or_commit_create() {
    let (runtime, _worker, fake, _commands, _events) = setup("jam-create-generic-refused");
    runtime.block_on(async {
        let kernel = Kernel::new(Arc::new(fake.clone()), Profile::ObservedFourFields);
        assert_eq!(
            Host::run(&kernel, Action::Create(CreateIntent::Requested))
                .await
                .err(),
            Some(Error::NotReady)
        );
        assert_eq!(fake.calls(), 0);
        assert!(kernel.snapshot().membership == Membership::Unknown);
        assert!(kernel.snapshot().reconcile_required);
    });
}

#[test]
fn diagnostic_preflight_refusals_are_local_and_do_not_change_the_guards() {
    use crate::backend::jam::diagnostic::Class;
    for error in [Error::NotReady, Error::Stale, Error::Busy] {
        let (runtime, mut worker, fake, mut commands, events) = setup("jam-diagnostic-refusal");
        runtime.block_on(async {
            let expected = worker.jam.gate.stamp();
            let mut request_stamp = expected;
            match error {
                Error::NotReady => worker.jam.gate.lock().listening = false,
                Error::Stale => request_stamp.operation = 99,
                Error::Busy => {
                    let _release = fake.wait();
                    request(&mut worker, Action::Current);
                }
                _ => unreachable!(),
            }
            let probe = Probe::default();
            let action = Action::DiagnoseCurrent(probe.clone());
            worker.jam.gate.arm_diagnostic(&action);
            worker.request_jam(Request {
                expected: request_stamp,
                action,
            });
            assert_eq!(
                probe.snapshot().class,
                Class::Failed(Stage::Preflight, error)
            );
            assert_eq!(latest(&events).snapshot.error, Some(error));
            assert_eq!(fake.calls(), 0);
            assert!(commands.try_recv().is_err());
        });
    }
}

#[test]
fn diagnostic_budget_timeout_and_transport_failure_never_retry_or_grant_absence() {
    use crate::backend::jam::diagnostic::Class;
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-diagnostic-budget");
    runtime.block_on(async {
        tokio::time::pause();
        let _held = Arc::clone(&worker.background_session)
            .acquire_many_owned(4)
            .await
            .unwrap();
        let probe = diagnose(&mut worker);
        tokio::task::yield_now().await;
        assert_eq!(probe.snapshot().class, Class::Waiting(Stage::Budget));
        tokio::time::advance(Duration::from_secs(21)).await;
        finish(&mut worker, &mut commands).await;
        assert_eq!(
            probe.snapshot().class,
            Class::Failed(Stage::Budget, Error::Timeout)
        );
        assert_eq!(latest(&events).snapshot.error, Some(Error::Timeout));
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!(fake.calls(), 0);
        assert_eq!(fake.writes(), 0);
    });
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-diagnostic-transport");
    runtime.block_on(async {
        let release = fake.wait();
        let probe = diagnose(&mut worker);
        tokio::task::yield_now().await;
        drop(release);
        finish(&mut worker, &mut commands).await;
        assert_eq!(
            probe.snapshot().class,
            Class::Failed(Stage::Transport, Error::Transport)
        );
        assert_eq!(latest(&events).snapshot.error, Some(Error::Transport));
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!((fake.calls(), fake.writes()), (1, 0));
        tokio::task::yield_now().await;
        assert_eq!(fake.calls(), 1);
    });
}

#[test]
fn diagnostic_completion_coalesced_by_invalidation_stays_retired_and_account_clean() {
    use crate::backend::jam::diagnostic::Class;
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-diagnostic-retire");
    runtime.block_on(async {
        fake.push(absent());
        let probe = diagnose(&mut worker);
        finish(&mut worker, &mut commands).await;
        let gate = worker.jam.gate();
        assert_eq!(gate.diagnostic().unwrap().class, Class::Absent);
        assert_eq!(probe.snapshot().http, Some(404));
        assert!(latest(&events).snapshot.membership == Membership::Absent);
        let stamp = gate.stamp();
        gate.signal(stamp.engine, Notice::Jam);
        let mut backend = consumer(&worker, events);
        let delivered = backend.poll_jam().unwrap();
        assert!(delivered.phase == Phase::Invalidated(Cause::Jam));
        assert!(delivered.snapshot.membership == Membership::Unknown);
        assert_eq!(
            backend.jam_diagnostic().unwrap().class,
            Class::Invalidated(Cause::Jam)
        );
        probe.status(200);
        probe.finish(Ok(&Membership::Absent));
        assert_eq!(probe.snapshot().http, Some(404));
        assert!(!gate.accepts(stamp));
        worker.jam.retire(stamp.account + 1, Cause::Account);
        assert!(gate.diagnostic().is_none());
        assert_eq!(probe.snapshot().http, None);
        assert_eq!(probe.snapshot().schema, Schema::None);
        assert_eq!(fake.calls(), 1);
    });
}

#[test]
fn diagnostic_queue_replacement_and_normal_checks_cannot_restore_an_old_slot() {
    use crate::backend::jam::diagnostic::Class;
    let (runtime, mut worker, fake, mut commands, _) = setup("jam-diagnostic-queue");
    runtime.block_on(async {
        let first = Probe::default();
        let old = Action::DiagnoseCurrent(first.clone());
        worker.jam.gate.arm_diagnostic(&old);
        assert_eq!(
            worker.jam.gate.diagnostic().unwrap().class,
            Class::Waiting(Stage::Queued)
        );
        let next = Probe::default();
        worker
            .jam
            .gate
            .arm_diagnostic(&Action::DiagnoseCurrent(next.clone()));
        fake.push(absent());
        request(&mut worker, old);
        finish(&mut worker, &mut commands).await;
        assert_eq!(worker.jam.gate.diagnostic().unwrap(), next.snapshot());
        assert_eq!(next.snapshot().class, Class::Waiting(Stage::Queued));
        worker.jam.gate.arm_diagnostic(&Action::Current);
        next.status(200);
        assert!(worker.jam.gate.diagnostic().is_none());
        assert_eq!(next.snapshot().http, None);
    });
}

#[test]
fn diagnostic_result_and_late_completion_never_replace_context_retirement() {
    use crate::backend::jam::diagnostic::Class;
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-diagnostic-late");
    runtime.block_on(async {
        fake.push(empty(200));
        let probe = diagnose(&mut worker);
        finish(&mut worker, &mut commands).await;
        assert_eq!(
            probe.snapshot().class,
            Class::Failed(Stage::Response, Error::InvalidResponse)
        );
        assert_eq!(probe.snapshot().schema, Schema::JsonStructure);
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert!(latest(&events).snapshot.reconcile_required);
        fake.push(owner());
        let probe = diagnose(&mut worker);
        let complete = queued(&mut commands).await;
        assert_eq!(probe.snapshot().class, Class::Present);
        assert_eq!(probe.snapshot().url_field, diagnostic::UrlField::Absent);
        // Verification can disappear before any lifecycle watch notification.
        worker.jam.gate.lock().verify = Arc::new(|| false);
        let sanitized = worker.jam.gate.diagnostic().unwrap();
        assert_eq!(sanitized.class, Class::Invalidated(Cause::Context));
        assert_eq!(sanitized.http, None);
        assert_eq!(sanitized.schema, Schema::None);
        assert_eq!(sanitized.url_field, diagnostic::UrlField::NotObserved);
        assert_eq!(
            sanitized.format_boundary,
            diagnostic::FormatBoundary::NotApplicable
        );
        worker.finish_jam(complete);
        probe.invitation_format(Err(diagnostic::FormatBoundary::TokenQuery));
        assert_eq!(probe.snapshot(), sanitized);
        assert!(latest(&events).snapshot.membership == Membership::Unknown);
        assert_eq!((fake.calls(), fake.writes()), (2, 0));
    });
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
fn listeners_are_reserved_on_the_real_dealer_before_spirc_launch_and_do_not_claim_readiness() {
    let (runtime, mut worker, _fake, _commands, events) = setup("jam-reserved-listeners");
    let _entered = runtime.enter();
    let session = Session::new(
        librespot_core::config::SessionConfig {
            device_id: "synthetic-device".into(),
            ..Default::default()
        },
        None,
    );
    session.set_username("alice");
    assert!(!session.dealer().handles("social-connect/v2/session_update"));
    assert!(!session.dealer().handles("hm://pusher/v1/connections/"));
    let prepared = PreparedListeners::new(session.clone());
    assert!(prepared.jam.is_ok() && prepared.connections.is_ok());
    assert!(session.dealer().handles("social-connect/v2/session_update"));
    assert!(session.dealer().handles("hm://pusher/v1/connections/"));
    // Spirc reserves these same prefixes. SubscriberMap supports both receivers;
    // this is not the exclusive request-handler map.
    let _spirc_jam = session
        .dealer()
        .add_listen_for("social-connect/v2/session_update")
        .unwrap();
    let _spirc_connections = session
        .dealer()
        .add_listen_for("hm://pusher/v1/connections/")
        .unwrap();
    worker.jam.attach(
        prepared,
        worker.session.subscribe(),
        Arc::clone(&worker.api),
        Arc::clone(&worker.background_session),
    );
    assert!(worker.jam.gate.lock().listening);
    assert!(latest(&events).snapshot.membership == Membership::Unknown);
    assert!(latest(&events).snapshot.reconcile_required);
    // No connection has been announced. Registered listeners are not permission
    // to skip SessionTransport's nonempty connection check or issue Current.
    let probe = diagnose(&mut worker);
    assert_eq!(
        probe.snapshot().to_string(),
        "class=preflight_not_ready; http=none; schema=none; source=deliberate_current; url_field=not_observed; format_boundary=not_applicable"
    );
    assert!(worker.jam.gate.lock().pending.is_none());
    assert!(latest(&events).snapshot.membership == Membership::Unknown);
}

#[test]
fn failed_listener_reservation_remains_blocked_when_connection_later_becomes_ready() {
    let (runtime, mut worker, _fake, _commands, events) = setup("jam-listener-install-failure");
    let _entered = runtime.enter();
    let session = Session::new(
        librespot_core::config::SessionConfig {
            device_id: "synthetic-device".into(),
            ..Default::default()
        },
        None,
    );
    session.set_username("alice");
    let mut prepared = PreparedListeners::new(session.clone());
    // Inject only the local reservation failure, without launching Dealer or
    // simulating its network. The private start-window is verified statically.
    prepared.jam = Err(librespot_core::Error::failed_precondition(
        "synthetic local reservation failure",
    ));
    worker.jam.attach(
        prepared,
        worker.session.subscribe(),
        Arc::clone(&worker.api),
        Arc::clone(&worker.background_session),
    );
    session.set_connection_id("synthetic-later-connection");
    let probe = diagnose(&mut worker);
    assert_eq!(
        probe.snapshot().to_string(),
        "class=preflight_not_ready; http=none; schema=none; source=deliberate_current; url_field=not_observed; format_boundary=not_applicable"
    );
    assert!(!worker.jam.gate.lock().listening);
    assert!(worker.jam.gate.lock().pending.is_none());
    assert!(worker.jam.gate.lock().kernel.is_none());
    assert!(latest(&events).snapshot.membership == Membership::Unknown);
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
fn queued_lifecycle_results_cannot_apply_after_retirement() {
    for kind in [Kind::Current, Kind::Create, Kind::Join, Kind::End] {
        let (runtime, mut worker, fake, mut commands, events) =
            setup(&format!("jam-late-{kind:?}"));
        runtime.block_on(async {
            if kind != Kind::Current {
                fake.push(if matches!(kind, Kind::Create | Kind::Join) {
                    absent()
                } else {
                    owner()
                });
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
            }
            let action = match kind {
                Kind::Join => {
                    let member = || Reply { status: 200, content_type: None, body: br#"{"session_id":"synthetic-jam","is_session_owner":false,"is_listening":true,"is_controlling":false}"#.to_vec() };
                    fake.push(member());
                    fake.push(member());
                    Action::Join {
                        link: Link::parse("https://open.spotify.com/socialsession/Synthetic123").unwrap(),
                        intent: JoinIntent::ListenAndControlRequested,
                    }
                },
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

#[test]
fn polling_releases_the_watch_borrow_before_waiting_for_the_lifecycle_gate() {
    let (_runtime, worker, _fake, _commands, events) = setup("jam-watch-lock-order");
    let mut backend = consumer(&worker, events);
    let gate = worker.jam.gate();
    let mut state = gate.lock();
    state.publish(Phase::Invalidated(Cause::Context), unknown(None));
    let event = Arc::clone(state.latest.as_ref().unwrap());
    let output = state.output.clone();
    let before = Arc::strong_count(&event);
    let poll = std::thread::spawn(move || backend.poll_jam());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while Arc::strong_count(&event) == before && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let cloned = Arc::strong_count(&event) > before;
    let (sent, received) = std::sync::mpsc::channel();
    let publisher = std::thread::spawn(move || {
        output.send_replace(None);
        sent.send(()).unwrap();
    });
    // The consumer is waiting for the gate held above. A publisher must still
    // be able to replace its watch value, as real publish() does under that gate.
    let published_without_gate = received.recv_timeout(Duration::from_secs(1)).is_ok();
    drop(state);
    publisher.join().unwrap();
    assert!(poll.join().unwrap().is_some());
    assert!(cloned, "the consumer must have cloned the pending event");
    assert!(
        published_without_gate,
        "polling held a watch read lock while waiting for the lifecycle gate"
    );
}

#[test]
fn uncertain_join_fence_survives_dealer_retirement_and_account_reuse() {
    for change in 0..3 {
        let (runtime, mut worker, fake, mut commands, events) = setup("jam-join-retirement");
        runtime.block_on(async {
            fake.push(absent());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let release = fake.wait();
            request(
                &mut worker,
                Action::Join {
                    link: Link::parse("https://open.spotify.com/socialsession/Synthetic123")
                        .unwrap(),
                    intent: JoinIntent::ListenAndControlRequested,
                },
            );
            tokio::task::yield_now().await;
            assert_eq!(fake.writes(), 1);
            assert!(
                worker
                    .jam
                    .gate
                    .lock()
                    .write_uncertain
                    .load(Ordering::SeqCst)
            );
            match change {
                0 => {
                    let gate = worker.jam.gate();
                    gate.signal(gate.stamp().engine, Notice::Jam);
                }
                1 => {
                    worker.take_engine_for_resume();
                    bind(&mut worker, &fake, "alice");
                }
                _ => {
                    worker.sign_out();
                    worker
                        .api
                        .install(ApiSource::Shared, AccountId::new("alice"))
                        .unwrap();
                    worker.signed_in = true;
                    bind(&mut worker, &fake, "alice");
                }
            }
            tokio::task::yield_now().await;
            assert!(release.send(empty(200)).is_err());
            fake.push(absent());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            assert_eq!(latest(&events).snapshot.error, Some(Error::WriteUncertain));
            assert!(latest(&events).snapshot.reconcile_required);
            request(
                &mut worker,
                Action::Join {
                    link: Link::parse("https://open.spotify.com/socialsession/Synthetic123")
                        .unwrap(),
                    intent: JoinIntent::ListenAndControlRequested,
                },
            );
            finish(&mut worker, &mut commands).await;
            assert_eq!(latest(&events).snapshot.error, Some(Error::WriteUncertain));
            assert_eq!(fake.writes(), 1);
        });
    }
}

#[test]
fn app_connect_actions_keep_backend_uncertainty_across_relogin_and_current_404() {
    use crate::app::{App, AppOptions};
    use crate::backend::{AuthStatus, Event as BackendEvent};
    use crate::model::{Action as UiAction, jam::JoinDraft};
    use crate::player::PlayerCommand;
    use crate::settings::Settings;

    for uncertain in [false, true] {
        let (runtime, mut worker, fake, mut commands, events) = setup("jam-app-relogin");
        runtime.block_on(async {
            let mut app = App::new(
                &Waker::default(),
                worker.dirs.clone(),
                Settings::default(),
                AppOptions {
                    media_controls: false,
                    restore_sign_in: false,
                    tray: false,
                },
            );
            app.backend.shutdown();
            app.backend = consumer(&worker, events);
            // The consumer worker is stopped. Commands go only to this spy,
            // while the real injected Jam Bridge remains owned by `worker`.
            let (spy, mut sent) = tokio::sync::mpsc::unbounded_channel();
            app.backend.commands = spy;
            let (auth, received) = std::sync::mpsc::channel();
            app.backend.events = received;
            let ctx = egui::Context::default();
            let frame = |app: &mut App| {
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    app.background_frame(ui.ctx())
                });
                output.textures_delta.clear();
            };
            let mut release = None;
            if uncertain {
                fake.push(absent());
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
                frame(&mut app);
                assert!(app.jam.can_create());
                app.jam.join = Some(JoinDraft {
                    link: "private-synthetic-draft".into(),
                    ..Default::default()
                });
                release = Some(fake.wait());
                request(
                    &mut worker,
                    Action::Join {
                        link: Link::parse("https://open.spotify.com/socialsession/Synthetic123")
                            .unwrap(),
                        intent: JoinIntent::ListenAndControlRequested,
                    },
                );
                tokio::task::yield_now().await;
                assert_eq!(fake.writes(), 1);
                assert!(app.backend.jam_write_uncertain());
            }
            worker.sign_out();
            auth.send(BackendEvent::Auth(AuthStatus::SignedOut))
                .unwrap();
            frame(&mut app); // Exercises handle_auth -> reset_data.
            worker
                .api
                .install(ApiSource::Shared, AccountId::new("alice"))
                .unwrap();
            worker.signed_in = true;
            bind(&mut worker, &fake, "alice");
            auth.send(BackendEvent::Auth(AuthStatus::Connected {
                username: "alice".into(),
            }))
            .unwrap();
            frame(&mut app); // The same account's login also calls reset_data.
            assert!(!app.jam.engaged);
            assert!(app.jam.membership == Membership::Unknown);
            assert!(app.jam.join.is_none() && app.jam.confirmation.is_none());
            assert!(!app.jam.fresh() && app.jam.owner_id().is_none());
            assert_eq!(app.backend.jam_write_uncertain(), uncertain);
            if let Some(release) = release {
                tokio::task::yield_now().await;
                assert!(release.send(empty(200)).is_err());
            }
            let receiver = crate::zeroconf::Receiver {
                name: "synthetic-receiver".into(),
                device_id: Some("synthetic-target".into()),
                address: "192.0.2.1".parse().unwrap(),
                port: 8009,
                path: "/".into(),
                cast: true,
            };
            for after_current in [false, true] {
                if after_current {
                    fake.push(absent());
                    request(&mut worker, Action::Current);
                    finish(&mut worker, &mut commands).await;
                    frame(&mut app);
                    assert!(app.jam.membership == Membership::Absent);
                    assert_eq!(app.jam.fresh(), !uncertain);
                    assert_eq!(app.jam.can_create(), !uncertain);
                    assert!(app.jam.owner_id().is_none() && app.jam.join.is_none());
                }
                while sent.try_recv().is_ok() {}
                app.backend.take_transfer_requests();
                app.backend.take_player_commands();
                app.activating_receiver = None;
                app.apply(UiAction::Transfer("synthetic-other-device".into()), &ctx);
                app.apply(UiAction::ActivateReceiver(Box::new(receiver.clone())), &ctx);
                let expected = usize::from(!uncertain);
                assert_eq!(
                    app.backend.take_transfer_requests().len(),
                    expected,
                    "Transfer: uncertain={uncertain}, after_current={after_current}"
                );
                assert!(
                    !app.backend
                        .take_player_commands()
                        .iter()
                        .any(|command| matches!(command, PlayerCommand::Transfer))
                );
                let mut activations = 0;
                while let Ok(command) = sent.try_recv() {
                    if matches!(command, Command::ActivateReceiver(_)) {
                        activations += 1;
                    }
                }
                assert_eq!(
                    activations, expected,
                    "ActivateReceiver: uncertain={uncertain}, after_current={after_current}"
                );
                assert_eq!(app.activating_receiver.is_some(), !uncertain);
            }
            assert_eq!(fake.writes(), usize::from(uncertain));
            app.backend.shutdown();
        });
    }
}

fn official_reply(value: &str) -> Reply {
    let mut reply = owner();
    let mut root: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    root["join_session_url"] = serde_json::json!(value);
    reply.body = serde_json::to_vec(&root).unwrap();
    reply
}
fn compare_official(worker: &mut Worker) -> Probe {
    let probe = Probe::with_official(
        Link::parse("https://open.spotify.com/socialsession/SyntheticSecret9").unwrap(),
    );
    let action = Action::DiagnoseCurrent(probe.clone());
    worker.jam.gate.arm_diagnostic(&action);
    request(worker, action);
    probe
}

#[test]
fn official_comparison_is_one_read_no_write_and_publishes_only_on_live_finish() {
    use super::diagnostic::{Comparison, OfficialRelation, Shape};
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-official-one-read");
    runtime.block_on(async {
        fake.push(owner());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        fake.push(official_reply("SyntheticSecret9"));
        let p = compare_official(&mut worker);
        let completed = queued(&mut commands).await;
        assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
        worker.finish_jam(completed);
        assert_eq!(
            p.snapshot().comparison,
            Some(Comparison {
                shape: Shape::BareAsciiAlnum,
                official_relation: OfficialRelation::Same
            })
        );
        assert_eq!((fake.calls(), fake.writes()), (2, 0));
        assert!(
            latest(&events).snapshot.metadata.invitation.is_none(),
            "comparison never supplies an invitation"
        );
        assert!(!worker.jam.gate.write_uncertain());
    });
}

#[test]
fn official_six_templates_use_one_current_and_seal_retired_or_late_delivery() {
    use super::diagnostic::{Comparison, OfficialRelation};
    for (value, shape) in super::super::comparison::template_fixtures("SyntheticSecret9") {
        for case in 0..4 {
            let (runtime, mut worker, fake, mut commands, events) = setup("jam-official-six");
            runtime.block_on(async {
                tokio::time::pause();
                fake.push(owner());
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
                fake.push(official_reply(&value));
                let p = compare_official(&mut worker);
                let completed = queued(&mut commands).await;
                assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
                match case {
                    1 => {
                        let gate = worker.jam.gate();
                        gate.signal(gate.stamp().engine, Notice::Jam);
                    }
                    2 => worker.jam.gate.cancel_comparison(),
                    3 => tokio::time::advance(Duration::from_secs(21)).await,
                    _ => (),
                }
                worker.finish_jam(completed);
                assert_eq!(
                    p.snapshot().comparison,
                    Some(if case == 0 {
                        Comparison {
                            shape,
                            official_relation: OfficialRelation::Same,
                        }
                    } else {
                        Comparison::default()
                    })
                );
                assert_eq!((fake.calls(), fake.writes()), (2, 0));
                assert!(latest(&events).snapshot.metadata.invitation.is_none());
                let output = p.snapshot().to_string();
                assert!(!output.contains("SyntheticSecret9") && !output.contains(&value));
                assert_eq!(output.split(';').count(), 8);
                assert!(output.len() < 384);
                assert!(
                    output.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"_=; ".contains(&b))
                );
                if case == 0 {
                    assert!(output.contains(&format!("shape={}", shape.label())));
                }
                assert!(!worker.jam.gate.write_uncertain());
            });
        }
    }
}

#[test]
fn official_comparison_requires_existing_fresh_owner_and_no_uncertain_write() {
    use super::diagnostic::Comparison;
    for case in 0..5 {
        let (runtime, mut worker, fake, mut commands, _events) = setup("jam-official-preflight");
        runtime.block_on(async {
            tokio::time::pause();
            if case != 0 {
                let mut reply = owner();
                if case == 1 {
                    let mut root: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
                    root["is_session_owner"] = serde_json::json!(false);
                    reply.body = serde_json::to_vec(&root).unwrap();
                }
                fake.push(reply);
                request(&mut worker, Action::Current);
                finish(&mut worker, &mut commands).await;
            }
            if case == 2 {
                tokio::time::advance(super::super::FRESH_FOR).await;
            }
            if case == 3 {
                worker
                    .jam
                    .gate
                    .lock()
                    .write_uncertain
                    .store(true, Ordering::SeqCst);
            }
            if case == 4 {
                fake.0.context.lock().unwrap().connection = "changed-before-intention".into();
            }
            let before = fake.calls();
            let p = compare_official(&mut worker);
            assert_eq!(fake.calls(), before);
            assert_eq!(fake.writes(), 0);
            assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
            assert!(commands.try_recv().is_err());
        });
    }
}

#[test]
fn official_comparison_cancel_budget_expiry_retirement_and_late_results_cannot_publish() {
    use super::diagnostic::Comparison;
    for case in 0..6 {
        let (runtime, mut worker, fake, mut commands, _events) = setup("jam-official-retire");
        runtime.block_on(async {
            tokio::time::pause();
            fake.push(owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let held = if case < 2 {
                Some(
                    Arc::clone(&worker.background_session)
                        .acquire_many_owned(4)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut release = if matches!(case, 2 | 3) {
                Some(fake.wait())
            } else {
                fake.push(official_reply("SyntheticSecret9"));
                None
            };
            let p = compare_official(&mut worker);
            if case < 2 {
                tokio::task::yield_now().await;
                assert_eq!(fake.calls(), 1);
                if case == 0 {
                    worker.jam.gate.cancel_comparison();
                } else {
                    tokio::time::advance(super::super::FRESH_FOR).await;
                }
                drop(held);
                finish(&mut worker, &mut commands).await;
                assert_eq!(fake.calls(), 1);
            } else if matches!(case, 2 | 3) {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while fake.calls() < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                if case == 2 {
                    let gate = worker.jam.gate();
                    gate.signal(gate.stamp().engine, Notice::Jam);
                    release.as_mut().unwrap().closed().await;
                    assert!(commands.try_recv().is_err());
                } else {
                    tokio::time::advance(Duration::from_secs(21)).await;
                    finish(&mut worker, &mut commands).await;
                }
                assert!(
                    release
                        .take()
                        .unwrap()
                        .send(official_reply("SyntheticSecret9"))
                        .is_err()
                );
            } else {
                let completed = queued(&mut commands).await;
                if case == 4 {
                    let gate = worker.jam.gate();
                    gate.signal(gate.stamp().engine, Notice::Jam);
                } else {
                    tokio::time::advance(Duration::from_secs(16)).await;
                }
                worker.finish_jam(completed);
            }
            assert_eq!(
                p.snapshot().comparison,
                Some(Comparison::default()),
                "case {case}"
            );
            assert_eq!(fake.writes(), 0);
            assert!(fake.calls() <= 2);
        });
    }
}

#[test]
fn official_gui_invalid_input_and_queued_cancel_cannot_issue_a_read_or_replay_input() {
    use crate::app::{App, AppOptions};
    use crate::model::{
        Action as UiAction, QueueTab,
        jam::{Intent, OfficialEdit, UiAction as JamAction},
    };
    let (runtime, mut worker, fake, mut commands, events) = setup("jam-official-gui-input");
    runtime.block_on(async {
        fake.push(owner());
        request(&mut worker, Action::Current);
        finish(&mut worker, &mut commands).await;
        let mut app = App::new(
            &Waker::default(),
            worker.dirs.clone(),
            crate::settings::Settings::default(),
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
                tray: false,
            },
        );
        app.backend.shutdown();
        app.backend = consumer(&worker, events);
        let (spy, mut sent) = tokio::sync::mpsc::unbounded_channel();
        app.backend.commands = spy;
        app.show_queue_panel = true;
        app.queue_tab = QueueTab::Queue;
        let ctx = egui::Context::default();
        let apply = |app: &mut App, intent| {
            let expected = worker.jam.gate.stamp();
            app.apply(UiAction::Jam(JamAction { intent, expected }), &ctx);
        };
        apply(&mut app, Intent::AskCompare);
        for value in [
            "SyntheticSecret9",
            "https://spotify.link/unsupported",
            "spotify://sessions/join/SyntheticSecret9",
            "https://open.spotify.com/socialsession/SyntheticSecret9?bad=x",
        ] {
            apply(
                &mut app,
                Intent::EditOfficial(OfficialEdit::new(value.into())),
            );
            apply(&mut app, Intent::CompareOfficial);
            assert!(sent.try_recv().is_err());
            let draft = app.jam.official.as_ref().unwrap();
            assert!(!draft.has_input() && draft.invalid);
        }
        let edit =
            OfficialEdit::new("https://open.spotify.com/socialsession/SyntheticSecret9".into());
        let cloned = edit.clone();
        apply(&mut app, Intent::EditOfficial(edit));
        assert!(cloned.take().is_none());
        apply(&mut app, Intent::CompareOfficial);
        assert!(app.jam.official.is_none());
        let Command::Jam(request) = sent.try_recv().unwrap() else {
            panic!("one diagnostic expected");
        };
        let Action::DiagnoseCurrent(p) = &request.action else {
            panic!("never a mutation");
        };
        assert!(p.is_comparison());
        apply(&mut app, Intent::CancelCompare);
        assert!(app.jam.official.is_none() && app.backend.jam_diagnostic().is_none());
        worker.request_jam(request);
        assert_eq!((fake.calls(), fake.writes()), (1, 0));
        assert!(sent.try_recv().is_err());
    });
}

#[test]
fn official_offline_or_closed_consumer_discards_input_without_http() {
    for offline in [true, false] {
        let (runtime, mut worker, fake, mut commands, events) = setup("jam-official-local-refusal");
        runtime.block_on(async {
            fake.push(owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let mut backend = consumer(&worker, events);
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            backend.commands = sender;
            drop(receiver);
            backend.offline = offline;
            let probe = Probe::with_official(
                super::super::Link::parse(
                    "https://open.spotify.com/socialsession/SyntheticSecret9",
                )
                .unwrap(),
            );
            backend.send(Command::Jam(Request {
                expected: worker.jam.gate.stamp(),
                action: Action::DiagnoseCurrent(probe.clone()),
            }));
            assert!(probe.comparison_deadline().is_none());
            assert_eq!(probe.snapshot().comparison, Some(Default::default()));
            assert_eq!((fake.calls(), fake.writes()), (1, 0));
        });
    }
}

#[test]
fn official_visibility_and_getter_loss_purge_native_input_before_actions_and_seal_late_results() {
    use crate::app::{App, AppOptions};
    use crate::model::{Action as UiAction, Page, QueueTab, jam::OfficialDraft};
    for case in 0..5 {
        let (runtime, mut worker, fake, mut commands, events) =
            setup("jam-official-surface-retire");
        runtime.block_on(async {
            fake.push(owner());
            request(&mut worker, Action::Current);
            finish(&mut worker, &mut commands).await;
            let mut app = App::new(
                &Waker::default(),
                worker.dirs.clone(),
                crate::settings::Settings::default(),
                AppOptions {
                    media_controls: false,
                    restore_sign_in: false,
                    tray: false,
                },
            );
            app.backend.shutdown();
            app.backend = consumer(&worker, events);
            let (spy, mut sent) = tokio::sync::mpsc::unbounded_channel();
            app.backend.commands = spy;
            app.history = vec![Page::Queue];
            app.history_index = 0;
            app.queue_tab = QueueTab::Queue;
            let ctx = egui::Context::default();
            app.apply(UiAction::SetQueueTab(QueueTab::Queue), &ctx);
            assert!(app.jam.owner_id().is_some());
            let probe;
            let completed;
            if case < 2 {
                probe = Probe::with_official(
                    Link::parse("https://open.spotify.com/socialsession/SyntheticSecret9").unwrap(),
                );
                worker
                    .jam
                    .gate
                    .arm_diagnostic(&Action::DiagnoseCurrent(probe.clone()));
                completed = None;
                let mut draft = OfficialDraft::default();
                draft.append("SyntheticSecret9".into());
                app.jam.official = Some(draft);
            } else {
                fake.push(official_reply("SyntheticSecret9"));
                probe = compare_official(&mut worker);
                completed = Some(queued(&mut commands).await);
            }
            let mut focus = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let response = ui.button("hidden capture");
                response.request_focus();
                focus = Some(response.id);
                ctx.data_mut(|data| {
                    data.insert_temp(egui::Id::new("jam-official-focus-id"), response.id)
                });
            });
            output.textures_delta.clear();
            let before = fake.calls();
            let stamp = worker.jam.gate.stamp();
            if matches!(case, 0 | 3) {
                app.lyrics_fullscreen = Some(false);
                app.show_queue_panel = false;
            } else if matches!(case, 1 | 4) {
                fake.0.context.lock().unwrap().connection = "getter-invalid-without-stamp".into();
                assert_eq!(worker.jam.gate.stamp(), stamp);
                if case == 1 {
                    assert!(app.jam.owner_id().is_some());
                }
            } else {
                app.history = vec![Page::Home];
                app.show_queue_panel = false;
            }
            let mut output =
                ctx.run_ui(
                    egui::RawInput {
                        events: vec![
                            egui::Event::Text("SyntheticText9".into()),
                            egui::Event::Paste("SyntheticPaste9".into()),
                        ],
                        ..Default::default()
                    },
                    |ui| {
                        ui.ctx().input(|input| {
                            for events in [&input.events, &input.raw.events] {
                                assert!(events.iter().any(|e| matches!(e, egui::Event::Text(_))));
                                assert!(events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
                            }
                        });
                        // This ordinary action invokes consume_jam exactly once before its
                        // own no-network tab update; a second consume cannot mask ordering.
                        app.apply(UiAction::SetQueueTab(QueueTab::Queue), ui.ctx());
                        assert!(app.jam.official.is_none());
                        assert!(!ui.ctx().memory(|memory| memory.has_focus(focus.unwrap())));
                        ui.ctx().input(|input| {
                            assert!(
                                !input.events.iter().chain(input.raw.events.iter()).any(
                                    |e| matches!(e, egui::Event::Text(_) | egui::Event::Paste(_))
                                )
                            )
                        });
                        assert!(app.actions.is_empty());
                    },
                );
            output.textures_delta.clear();
            assert!(probe.comparison_deadline().is_none());
            assert!(app.backend.jam_diagnostic().is_none());
            if let Some(completed) = completed {
                worker.finish_jam(completed);
            }
            assert_eq!(probe.snapshot().comparison, Some(Default::default()));
            assert!(app.backend.jam_diagnostic().is_none());
            assert_eq!(fake.calls(), before);
            assert_eq!(fake.writes(), 0);
            assert!(sent.try_recv().is_err());
            app.lyrics_fullscreen = None;
            app.history = vec![Page::Queue];
            app.apply(UiAction::SetQueueTab(QueueTab::Queue), &ctx);
            assert!(app.jam.official.is_none());
            assert!(sent.try_recv().is_err());
        });
    }
}
