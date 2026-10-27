use super::*;
use crate::backend::jam::tests::{Fake, Step, context, session, status};

fn current(gate: serde_json::Value) -> Reply {
    let mut reply = session("synthetic-jam", false);
    let mut root: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    root["redirectCommands"] = gate;
    reply.body = serde_json::to_vec(&root).unwrap();
    reply
}
fn budget() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(1))
}
fn paused() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| true)
}
fn pending<F: Future>(future: Pin<&mut F>) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(future.poll(&mut cx).is_pending());
}

#[test]
fn own_social_constructor_escapes_local_descriptor_and_has_exact_body_headers() {
    let mut ctx = context();
    ctx.device = "local/device ?#%é".into();
    let exchange = Exchange::Social {
        session: "synthetic-jam".into(),
        command_id: "0123456789abcdef0123456789abcdef".into(),
    };
    let req = request(
        &exchange,
        &ctx,
        "https://synthetic.invalid:443/old?secret=bad#old",
        "XX",
        "Bearer synthetic",
        "synthetic-client",
    )
    .unwrap();
    assert_eq!(req.method(), Method::POST);
    assert_eq!(
        req.uri().path(),
        "/social-connect/v2/sessions/synthetic-jam/commands/from/local%2Fdevice%20%3F%23%25%C3%A9"
    );
    assert!(req.uri().query().is_none());
    assert_eq!(req.headers()["authorization"], "Bearer synthetic");
    assert_eq!(req.headers()["client-token"], "synthetic-client");
    for name in ["authorization", "client-token", "spotify-connection-id"] {
        assert!(req.headers()[name].is_sensitive());
    }
    assert_eq!(req.headers()["spotify-connection-id"], ctx.connection);
    assert_eq!(req.headers()["content-type"], "application/json");
    assert_eq!(req.headers()["accept"], "application/json");
    assert_eq!(
        req.headers()["content-length"],
        req.body().len().to_string()
    );
    let body: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"command":{"endpoint":"resume","logging_params":{
        "command_id":"0123456789abcdef0123456789abcdef","interaction_ids":[],"page_instance_ids":[]}}})
    );
    for base in [
        "http://synthetic.invalid",
        "https://user:pass@synthetic.invalid",
        "invalid",
    ] {
        assert!(
            request(
                &exchange,
                &ctx,
                base,
                "XX",
                "Bearer synthetic",
                "synthetic-client"
            )
            .is_err()
        );
    }
    for command_id in ["short".to_owned(), "g".repeat(32)] {
        let invalid = Exchange::Social {
            session: "synthetic-jam".into(),
            command_id,
        };
        assert!(
            request(
                &invalid,
                &ctx,
                "https://synthetic.invalid",
                "XX",
                "Bearer synthetic",
                "synthetic-client"
            )
            .is_err()
        );
    }
    ctx.device = "..".into();
    assert!(
        request(
            &exchange,
            &ctx,
            "https://synthetic.invalid",
            "XX",
            "Bearer synthetic",
            "synthetic-client"
        )
        .is_err()
    );
}

#[test]
fn real_current_parser_retains_only_literal_bool_and_rejects_ambiguous_json() {
    for (extra, expected) in [
        ("\"redirectCommands\":true", Some(Gate::Enabled)),
        ("\"redirectCommands\":false", Some(Gate::Disabled)),
        ("\"redirectCommands\":null", Some(Gate::InvalidType)),
        ("\"redirectCommands\":\"true\"", Some(Gate::InvalidType)),
        ("\"redirect_commands\":true", Some(Gate::SnakeOnly)),
        (
            "\"redirectCommands\":true,\"redirect_commands\":true",
            Some(Gate::Conflict),
        ),
        ("\"redirectCommands\":true,\"redirectCommands\":true", None),
    ] {
        let body = format!(
            "{{\"session_id\":\"synthetic\",\"is_session_owner\":false,\"is_listening\":true,\"is_controlling\":false,{extra}}}"
        );
        let read = read_reply(
            Reply {
                status: 200,
                content_type: None,
                body: body.into_bytes(),
            },
            &Profile::ObservedFourFields,
        );
        assert_eq!(read.ok().map(|r| r.resume_gate), expected);
    }
    assert!(matches!(
        read_reply(session("synthetic", false), &Profile::ObservedFourFields)
            .unwrap()
            .resume_gate,
        Gate::Missing
    ));
}

#[tokio::test]
async fn dispatcher_one_current_one_post_never_replenishes_after_invalidation() {
    let fake = Fake::new(vec![current(true.into()), status(200)]);
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_eq!(intent.command_id.len(), 32);
    let id = intent.command_id.clone();
    let result = kernel.experimental_resume(intent, budget()).await;
    assert_eq!(
        result,
        Outcome {
            terminal: Terminal::Accepted,
            gate: Gate::Enabled,
            current_possible: true,
            post_possible: true
        }
    );
    kernel.invalidate();
    let (intent, _cancel2) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_ne!(id, intent.command_id);
    assert_eq!(
        kernel.experimental_resume(intent, budget()).await.terminal,
        Terminal::Exhausted
    );
    assert_eq!((fake.count(), fake.writes()), (2, 1));
    let replacement = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel3) = replacement
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_eq!(
        replacement
            .experimental_resume(intent, budget())
            .await
            .terminal,
        Terminal::Exhausted
    );
    // Ordinary Current remains usable. The experiment does not globally block it.
    fake.push(Step::Reply(session("synthetic-next", false)));
    assert!(kernel.current().await.is_ok());
    assert_eq!(fake.count(), 3);
}

#[tokio::test]
async fn unknown_false_absent_and_http_current_never_post() {
    for reply in [
        current(false.into()),
        current(serde_json::Value::Null),
        session("synthetic", false),
        status(404),
        status(401),
    ] {
        let fake = Fake::new(vec![reply]);
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let (intent, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let result = kernel.experimental_resume(intent, budget()).await;
        assert_ne!(result.terminal, Terminal::Accepted);
        assert!(result.current_possible && !result.post_possible);
        assert_eq!((fake.count(), fake.writes()), (1, 0));
    }
}

#[tokio::test(start_paused = true)]
async fn preparation_cancellation_context_pause_and_deadline_revalidate_both_phases() {
    for social in [false, true] {
        for change in 0..7 {
            let fake = Fake::new(if social {
                vec![current(true.into())]
            } else {
                vec![]
            });
            let (send, receive) = tokio::sync::oneshot::channel();
            fake.push(Step::Preparing(
                receive,
                if social {
                    status(200)
                } else {
                    current(true.into())
                },
            ));
            let authorization = Authorization::explicitly_authorized();
            let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
            let paused_state = Arc::new(AtomicBool::new(true));
            let paused_copy = Arc::clone(&paused_state);
            let (intent, cancel) = kernel
                .experimental_resume_intent(
                    &authorization,
                    Arc::new(move || paused_copy.load(Ordering::SeqCst)),
                )
                .unwrap();
            let future = kernel.experimental_resume(intent, budget());
            tokio::pin!(future);
            pending(future.as_mut());
            match change {
                0 => cancel.cancel(),
                1 => kernel.invalidate(),
                2 => fake
                    .context
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .connection
                    .push('x'),
                3 => fake
                    .context
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .account
                    .push('x'),
                4 => paused_state.store(false, Ordering::SeqCst),
                5 => tokio::time::advance(DEADLINE).await,
                6 => {
                    fake.context.lock().unwrap().as_mut().unwrap().generation += 1;
                }
                _ => unreachable!(),
            }
            let _ = send.send(());
            let result = future.await;
            assert!(!result.post_possible);
            assert_ne!(result.terminal, Terminal::Accepted);
            assert_eq!(fake.writes(), 0);
            assert_eq!(fake.count(), usize::from(social));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn social_preparation_does_not_renew_current_ttl() {
    let fake = Fake::new(vec![current(true.into())]);
    let (send, receive) = tokio::sync::oneshot::channel();
    fake.push(Step::Preparing(receive, status(200)));
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let future = kernel.experimental_resume(intent, budget());
    tokio::pin!(future);
    pending(future.as_mut());
    tokio::time::advance(FRESH_FOR).await;
    send.send(()).unwrap();
    let result = future.await;
    assert_eq!(result.terminal, Terminal::Invalidated);
    assert_eq!(result.gate, Gate::Unknown);
    assert!(!result.post_possible);
    assert_eq!(fake.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn budget_wait_cancel_deadline_and_competition_never_start_auth_or_current() {
    for cancel_it in [false, true] {
        let fake = Fake::new(vec![]);
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let slots = budget();
        let held = slots.acquire().await.unwrap();
        let (intent, cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let future = kernel.experimental_resume(intent, Arc::clone(&slots));
        tokio::pin!(future);
        pending(future.as_mut());
        let (other, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        assert_eq!(
            kernel.experimental_resume(other, budget()).await.terminal,
            Terminal::Busy
        );
        if cancel_it {
            cancel.cancel();
        } else {
            tokio::time::advance(DEADLINE).await;
        }
        let result = future.await;
        assert!(!result.current_possible && !result.post_possible);
        assert_eq!(fake.count(), 0);
        drop(held);
        assert_eq!(slots.available_permits(), 1);
    }
}

#[tokio::test]
async fn cancellation_drops_pending_auth_without_polling_in_background() {
    let fake = Fake::new(vec![]);
    let (send, receive) = tokio::sync::oneshot::channel();
    fake.push(Step::Preparing(receive, current(true.into())));
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let future = kernel.experimental_resume(intent, budget());
    tokio::pin!(future);
    pending(future.as_mut());
    cancel.cancel();
    assert_eq!(future.await.terminal, Terminal::Cancelled);
    assert!(send.send(()).is_err());
    assert_eq!(fake.count(), 0);
}

#[tokio::test]
async fn late_post_reply_and_cancel_cannot_restore_authority_or_allow_replay() {
    for cancel_it in [false, true] {
        let fake = Fake::new(vec![current(true.into())]);
        let (send, receive) = tokio::sync::oneshot::channel();
        fake.push(Step::Wait(receive));
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let (intent, cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let future = kernel.experimental_resume(intent, budget());
        tokio::pin!(future);
        pending(future.as_mut());
        if cancel_it {
            cancel.cancel();
        } else {
            kernel.invalidate();
        }
        let _ = send.send(status(200));
        let result = future.await;
        assert_ne!(result.terminal, Terminal::Accepted);
        assert!(result.post_possible);
        assert_eq!(result.gate, Gate::Unknown);
        assert_eq!(fake.writes(), 1);
        assert!(matches!(kernel.snapshot().membership, Membership::Unknown));
        let (intent, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        assert_eq!(
            kernel.experimental_resume(intent, budget()).await.terminal,
            Terminal::Exhausted
        );
    }
}

#[tokio::test(start_paused = true)]
async fn uncertain_post_timeout_does_not_retry_and_releases_budget() {
    let fake = Fake::new(vec![current(true.into())]);
    fake.push(Step::Pending);
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let slots = budget();
    let future = kernel.experimental_resume(intent, Arc::clone(&slots));
    tokio::pin!(future);
    pending(future.as_mut());
    tokio::time::advance(DEADLINE).await;
    let result = future.await;
    assert_eq!(result.terminal, Terminal::Deadline);
    assert!(result.post_possible);
    assert_eq!(
        (fake.count(), fake.writes(), slots.available_permits()),
        (2, 1, 1)
    );
}

#[tokio::test]
async fn http_rejections_and_transport_errors_are_terminal_without_local_fallback() {
    for code in [200, 401, 403, 404, 410, 429, 500, 302] {
        let fake = Fake::new(vec![current(true.into()), status(code)]);
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let (intent, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let result = kernel.experimental_resume(intent, budget()).await;
        assert_eq!(
            result.terminal,
            if code == 200 {
                Terminal::Accepted
            } else {
                Terminal::Http(code)
            }
        );
        assert_eq!((fake.count(), fake.writes()), (2, 1));
    }
    let fake = Fake::new(vec![current(true.into())]);
    fake.push(Step::Error(Error::Transport));
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_eq!(
        kernel.experimental_resume(intent, budget()).await.terminal,
        Terminal::Transport
    );
    assert_eq!(fake.writes(), 1);
}

#[tokio::test]
async fn wrong_kernel_and_invalidation_before_first_poll_never_dispatch() {
    let fake = Fake::new(vec![]);
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let other = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    assert_eq!(
        other.experimental_resume(intent, budget()).await.terminal,
        Terminal::Invalidated
    );
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    kernel.invalidate();
    assert_eq!(
        kernel.experimental_resume(intent, budget()).await.terminal,
        Terminal::Invalidated
    );
    assert_eq!(fake.count(), 0);
}

#[tokio::test]
async fn same_poll_auth_result_cancelled_before_return_is_discarded() {
    let retired = AtomicBool::new(false);
    let check = || {
        if retired.load(Ordering::SeqCst) {
            Err(Terminal::Cancelled)
        } else {
            Ok(())
        }
    };
    let result = fenced(
        async {
            retired.store(true, Ordering::SeqCst);
            "synthetic-sensitive-result"
        },
        &check,
    )
    .await;
    assert_eq!(result, Err(Terminal::Cancelled));
}

#[test]
fn privacy_requires_silent_log_and_no_tracing_subscriber() {
    assert!(logging_safe(log::LevelFilter::Off, false));
    for level in [
        log::LevelFilter::Error,
        log::LevelFilter::Warn,
        log::LevelFilter::Info,
        log::LevelFilter::Debug,
        log::LevelFilter::Trace,
    ] {
        assert!(!logging_safe(level, false));
    }
    assert!(!logging_safe(log::LevelFilter::Off, true));
    let output = format!(
        "{:?}",
        Outcome {
            terminal: Terminal::Preparation,
            gate: Gate::Unknown,
            current_possible: false,
            post_possible: false
        }
    );
    assert!(!output.contains("synthetic-sensitive-result"));
}

#[tokio::test]
async fn dropping_attempt_releases_pending_and_budget_without_renewing_grant() {
    let fake = Fake::new(vec![]);
    let (send, receive) = tokio::sync::oneshot::channel();
    fake.push(Step::Preparing(receive, current(true.into())));
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let slots = budget();
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let mut future = Box::pin(kernel.experimental_resume(intent, Arc::clone(&slots)));
    pending(future.as_mut());
    drop(future);
    assert_eq!(slots.available_permits(), 1);
    assert!(send.send(()).is_err());
    let (intent, _cancel2) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_eq!(
        kernel.experimental_resume(intent, slots).await.terminal,
        Terminal::Exhausted
    );
    assert_eq!(fake.count(), 0);
}

#[tokio::test]
async fn preparation_failure_is_not_sent_and_does_not_renew_grant() {
    let fake = Fake::new(vec![]);
    let (send, receive) = tokio::sync::oneshot::channel();
    drop(send);
    fake.push(Step::Preparing(receive, current(true.into())));
    let authorization = Authorization::explicitly_authorized();
    let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
    let (intent, _cancel) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    let result = kernel.experimental_resume(intent, budget()).await;
    assert_eq!(
        result,
        Outcome {
            terminal: Terminal::Preparation,
            gate: Gate::Unknown,
            current_possible: false,
            post_possible: false
        }
    );
    let (intent, _cancel2) = kernel
        .experimental_resume_intent(&authorization, paused())
        .unwrap();
    assert_eq!(
        kernel.experimental_resume(intent, budget()).await.terminal,
        Terminal::Exhausted
    );
    assert_eq!(fake.count(), 0);
}

#[tokio::test]
async fn drop_handle_during_preparation_poll_blocks_current_and_social_dispatch() {
    for social in [false, true] {
        let fake = Fake::new(if social {
            vec![current(true.into())]
        } else {
            vec![]
        });
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let slots = budget();
        let (intent, cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let closed = intent.cancelled.clone();
        // This callback runs inside the preparation's poll, after select's
        // changed() branch was Pending, and returns Ready without yielding.
        fake.push(Step::PreparingPoll(
            Box::new(move || drop(cancel)),
            if social {
                status(200)
            } else {
                current(true.into())
            },
        ));
        fake.push(Step::Reply(status(200))); // Lets the unfixed path finish too.
        let result = kernel.experimental_resume(intent, Arc::clone(&slots)).await;
        assert!(
            !*closed.borrow(),
            "Drop must not be replaced by an explicit bool write"
        );
        assert!(closed.has_changed().is_err());
        assert_eq!(
            result,
            Outcome {
                terminal: Terminal::Cancelled,
                gate: Gate::Unknown,
                current_possible: social,
                post_possible: false
            }
        );
        assert_eq!((fake.count(), fake.writes()), (usize::from(social), 0));
        assert_eq!(slots.available_permits(), 1);
        assert!(kernel.gate.try_lock().is_ok());
        let (again, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        assert_eq!(
            kernel.experimental_resume(again, slots).await.terminal,
            Terminal::Exhausted
        );
        assert_eq!(fake.count(), usize::from(social));
    }
}

#[tokio::test]
async fn drop_handle_during_reply_poll_discards_retired_current_and_post_results() {
    for social in [false, true] {
        let fake = Fake::new(if social {
            vec![current(true.into())]
        } else {
            vec![]
        });
        let authorization = Authorization::explicitly_authorized();
        let kernel = Kernel::new(Arc::clone(&fake), Profile::ObservedFourFields);
        let slots = budget();
        let (intent, cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        let closed = intent.cancelled.clone();
        // The request boundary was crossed; retire the surface inside the
        // response poll immediately before returning its successful reply.
        fake.push(Step::ReplyPoll(
            Box::new(move || drop(cancel)),
            if social {
                status(200)
            } else {
                current(true.into())
            },
        ));
        fake.push(Step::Reply(status(200)));
        let result = kernel.experimental_resume(intent, Arc::clone(&slots)).await;
        assert!(!*closed.borrow());
        assert!(closed.has_changed().is_err());
        assert_eq!(
            result,
            Outcome {
                terminal: Terminal::Cancelled,
                gate: Gate::Unknown,
                current_possible: true,
                post_possible: social
            }
        );
        let expected_calls = 1 + usize::from(social);
        assert_eq!(
            (fake.count(), fake.writes()),
            (expected_calls, usize::from(social))
        );
        assert!(matches!(kernel.snapshot().membership, Membership::Unknown));
        assert_eq!(slots.available_permits(), 1);
        assert!(kernel.gate.try_lock().is_ok());
        let (again, _cancel) = kernel
            .experimental_resume_intent(&authorization, paused())
            .unwrap();
        assert_eq!(
            kernel.experimental_resume(again, slots).await.terminal,
            Terminal::Exhausted
        );
        assert_eq!(fake.count(), expected_calls);
    }
}
