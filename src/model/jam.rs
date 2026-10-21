//! Ephemeral Jam presentation. No network, persisted identity or inferred capabilities.
use crate::backend::jam::{
    Error, Membership,
    lifecycle::{Event, Kind, Phase, Stamp},
};

#[derive(Clone, Debug)]
pub enum Intent {
    Current,
    Create,
    AskEnd,
    CancelEnd,
    ConfirmEnd,
    Close,
    Manage,
    AskJoin,
    CancelJoin,
    RequestListenHere,
    ConfirmJoin,
    EditJoin(DraftText),
}

#[derive(Clone)]
pub struct DraftText(pub String);
impl std::fmt::Debug for DraftText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[private invitation]")
    }
}

#[derive(Clone, Debug)]
pub struct UiAction {
    pub intent: Intent,
    pub expected: Stamp,
}

pub struct Confirmation {
    pub stamp: Stamp,
    pub session_id: String,
}

/// Ephemeral private draft. Text is changed by actions after drawing.
#[derive(Default)]
pub struct JoinDraft {
    pub link: String,
    pub listen_here: bool,
    pub invalid: bool,
}

pub struct State {
    pub stamp: Stamp,
    pub engaged: bool,
    pub membership: Membership,
    pub pending: Option<Kind>,
    pub error: Option<Error>,
    pub reconcile: bool,
    pub fresh_until: Option<tokio::time::Instant>,
    pub confirmation: Option<Confirmation>,
    pub notice: Option<&'static str>,
    pub join: Option<JoinDraft>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            stamp: Stamp::default(),
            engaged: false,
            membership: Membership::Unknown,
            pending: None,
            error: None,
            reconcile: true,
            fresh_until: None,
            confirmation: None,
            notice: None,
            join: None,
        }
    }
}

impl State {
    pub fn sync(&mut self, live: Stamp) {
        if self.stamp != live {
            if self.pending.is_some()
                && self.stamp.account == live.account
                && self.stamp.engine == live.engine
                && self.stamp.revision == live.revision
                && live.operation == self.stamp.operation.wrapping_add(1)
            {
                // The backend may accept the intention between poll_jam() and
                // jam_stamp(). Hold its optimistic progress until that watch
                // event arrives, without reviving any write authority.
                self.stamp = live;
                self.fresh_until = None;
                self.reconcile = true;
                self.error = None;
                self.confirmation = None;
                self.join = None;
                return;
            }
            let notice = self
                .confirmation
                .is_some()
                .then_some("The Jam changed. Check its state before continuing.");
            *self = Self {
                stamp: live,
                notice,
                engaged: self.engaged,
                ..Self::default()
            };
        }
    }

    pub fn observe(&mut self, event: &Event, live: Stamp) -> bool {
        if event.stamp != live {
            return false;
        }
        // An operation increment within the same binding preserves the previous
        // projection only as pending, never as authority.
        let same_context = self.stamp.account == live.account
            && self.stamp.engine == live.engine
            && self.stamp.revision == live.revision;
        if !same_context {
            self.sync(live);
        }
        self.engaged |= matches!(event.phase, Phase::Pending(_) | Phase::Complete(_));
        self.stamp = live;
        self.confirmation = None;
        self.join = None;
        self.error = event.snapshot.error;
        self.reconcile = event.snapshot.reconcile_required;
        self.fresh_until = event.snapshot.fresh_until;
        self.pending = match event.phase {
            Phase::Pending(kind) => Some(kind),
            _ => None,
        };
        if self.pending.is_none() {
            if !same_context || !matches!(event.snapshot.membership, Membership::Unknown) {
                self.membership = event.snapshot.membership.clone();
            }
            self.notice = if matches!(event.phase, Phase::Complete(Kind::End))
                && self.error.is_none()
                && matches!(self.membership, Membership::Absent)
            {
                Some("You no longer participate in this Jam. Its state for others is unconfirmed.")
            } else {
                self.notice.take()
            };
        }
        true
    }

    pub fn fresh(&self) -> bool {
        self.pending.is_none()
            && !self.reconcile
            && self
                .fresh_until
                .is_some_and(|until| tokio::time::Instant::now() < until)
    }
    pub fn can_create(&self) -> bool {
        self.fresh() && matches!(self.membership, Membership::Absent)
    }
    pub fn owner_id(&self) -> Option<&str> {
        match &self.membership {
            Membership::Present(p) if self.fresh() && p.is_session_owner => Some(&p.session_id),
            _ => None,
        }
    }
    pub fn blocks_transfer(&self) -> bool {
        !self.fresh() || !matches!(self.membership, Membership::Absent)
    }
    pub fn start(&mut self, kind: Kind) {
        self.engaged = true;
        self.pending = Some(kind);
        self.confirmation = None;
        self.error = None;
        self.notice = None;
        self.join = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::jam::{Projection, Snapshot};
    fn owner() -> Membership {
        Membership::Present(Projection {
            session_id: "synthetic-session".into(),
            is_session_owner: true,
            is_listening: true,
            is_controlling: false,
        })
    }
    fn event(stamp: Stamp, membership: Membership) -> Event {
        Event {
            stamp,
            phase: Phase::Complete(Kind::Current),
            snapshot: Snapshot {
                membership,
                reconcile_required: false,
                error: None,
                fresh_until: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(15)),
            },
        }
    }
    #[test]
    fn join_draft_and_confirmation_do_not_survive_context_reuse() {
        let mut state = State {
            join: Some(JoinDraft {
                link: "private-synthetic".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let old = state.stamp;
        state.sync(Stamp { revision: 1, ..old });
        state.sync(old);
        assert!(state.join.is_none());
        let private = DraftText("private-synthetic".into());
        assert!(!format!("{:?}", Intent::EditJoin(private)).contains("private-synthetic"));
    }

    #[test]
    fn stale_account_engine_revision_and_operation_cannot_restore_authority() {
        let live = Stamp {
            account: 2,
            engine: 3,
            revision: 4,
            operation: 5,
        };
        for old in [
            Stamp { account: 1, ..live },
            Stamp { engine: 2, ..live },
            Stamp {
                revision: 3,
                ..live
            },
            Stamp {
                operation: 4,
                ..live
            },
        ] {
            let mut state = State::default();
            state.sync(live);
            assert!(!state.observe(&event(old, owner()), live));
            assert!(state.owner_id().is_none());
        }
    }
    #[test]
    fn context_loss_discards_confirmation_and_does_not_restore_on_return() {
        let mut state = State::default();
        let stamp = state.stamp;
        state.observe(&event(stamp, owner()), stamp);
        state.confirmation = Some(Confirmation {
            stamp,
            session_id: "synthetic-session".into(),
        });
        state.sync(Stamp {
            revision: 1,
            ..stamp
        });
        assert!(state.confirmation.is_none());
        assert!(state.owner_id().is_none());
        assert!(state.engaged && state.blocks_transfer());
        state.sync(stamp);
        assert!(state.owner_id().is_none());
    }
    #[test]
    fn participant_never_has_end_and_absence_alone_enables_create() {
        let mut state = State::default();
        let stamp = state.stamp;
        let mut participant = owner();
        if let Membership::Present(p) = &mut participant {
            p.is_session_owner = false;
        }
        state.observe(&event(stamp, participant), stamp);
        assert!(state.owner_id().is_none());
        assert!(!state.can_create());
        state.observe(&event(stamp, Membership::Absent), stamp);
        assert!(state.can_create());
        assert!(!state.blocks_transfer());
        state.reconcile = true;
        assert!(state.blocks_transfer());
        state.reconcile = false;
        state.fresh_until = Some(tokio::time::Instant::now());
        assert!(state.blocks_transfer());
    }
    #[test]
    fn pending_preserves_projection_without_authority_and_error_cannot_confirm_it() {
        let mut state = State::default();
        let stamp = state.stamp;
        state.observe(&event(stamp, owner()), stamp);
        state.start(Kind::End);
        let next = Stamp {
            operation: 1,
            ..stamp
        };
        let mut pending = event(next, Membership::Unknown);
        pending.phase = Phase::Pending(Kind::End);
        pending.snapshot.reconcile_required = true;
        pending.snapshot.fresh_until = None;
        state.observe(&pending, next);
        assert!(matches!(state.membership, Membership::Present(_)));
        assert!(state.owner_id().is_none());
        pending.phase = Phase::Complete(Kind::End);
        pending.snapshot.error = Some(Error::Timeout);
        state.observe(&pending, next);
        assert!(matches!(state.membership, Membership::Present(_)));
        assert!(!state.fresh());
        assert!(state.owner_id().is_none());
        assert!(state.notice.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn freshness_uses_kernel_deadline_including_delivery_delay() {
        let mut state = State::default();
        let stamp = state.stamp;
        let delayed = event(stamp, owner());
        tokio::time::advance(std::time::Duration::from_secs(16)).await;
        state.observe(&delayed, stamp);
        assert!(state.owner_id().is_none());
        assert!(!state.can_create());
    }
    #[test]
    fn own_absence_after_end_does_not_claim_global_termination() {
        let mut state = State::default();
        let stamp = state.stamp;
        let mut ended = event(stamp, Membership::Absent);
        ended.phase = Phase::Complete(Kind::End);
        state.observe(&ended, stamp);
        assert!(state.notice.unwrap().contains("for others is unconfirmed"));
        assert!(state.owner_id().is_none());
    }
    #[test]
    fn pending_intent_survives_a_watch_stamp_race_without_authority() {
        for kind in [Kind::Current, Kind::Create, Kind::End] {
            let mut state = State::default();
            let expected = state.stamp;
            let previous = if kind == Kind::Create {
                Membership::Absent
            } else {
                owner()
            };
            state.observe(&event(expected, previous.clone()), expected);
            state.start(kind);
            let accepted = Stamp {
                operation: 1,
                ..expected
            };
            state.sync(accepted);
            assert_eq!(state.pending, Some(kind));
            assert!(state.membership == previous);
            assert!(!state.fresh());
            assert!(state.owner_id().is_none());
            assert!(!state.can_create());
            let result = if kind == Kind::End {
                Membership::Absent
            } else {
                owner()
            };
            let mut completed = event(accepted, result.clone());
            completed.phase = Phase::Complete(kind);
            assert!(state.observe(&completed, accepted));
            assert!(state.pending.is_none());
            assert!(state.membership == result);
            assert!(state.fresh());
        }
    }

    #[test]
    fn pending_is_discarded_when_an_acceptance_counter_also_changes_binding() {
        for live in [
            Stamp {
                account: 1,
                operation: 1,
                ..Stamp::default()
            },
            Stamp {
                engine: 1,
                operation: 1,
                ..Stamp::default()
            },
            Stamp {
                revision: 1,
                operation: 1,
                ..Stamp::default()
            },
        ] {
            let mut state = State::default();
            let expected = state.stamp;
            state.observe(&event(expected, owner()), expected);
            state.start(Kind::End);
            state.sync(live);
            assert!(state.pending.is_none());
            assert!(state.membership == Membership::Unknown);
            assert!(state.owner_id().is_none());
        }
    }
}
