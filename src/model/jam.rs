//! Ephemeral Jam presentation. No network, persisted identity or inferred capabilities.
use crate::backend::jam::{
    Error, Membership,
    lifecycle::{Event, Kind, Phase, Stamp},
};

#[derive(Clone)]
pub enum Intent {
    Resume(tokio::time::Instant),
    CancelResume,
    AskGeneration,
    ConfirmGeneration,
    CancelGeneration,
    Current,
    DiagnoseCurrent,
    AskCompare,
    EditOfficial(OfficialEdit),
    PasteOfficial(OfficialEdit),
    BackspaceOfficial,
    ClearOfficial,
    CompareOfficial,
    CancelCompare,
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
    OpenInvitation,
    CloseInvitation,
    ExpandQr,
    CloseQr,
    CopyInvitation,
}

// No Debug/Serialize. Clones share a one-use edit, never copy its private string.
#[derive(Clone)]
pub struct OfficialEdit(std::sync::Arc<std::sync::Mutex<Option<String>>>);
impl OfficialEdit {
    pub(crate) fn new(value: String) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(value))))
    }
    pub(crate) fn take(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}
impl std::fmt::Debug for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Resume(_) => "Resume",
            Self::CancelResume => "CancelResume",
            Self::AskGeneration => "AskGeneration",
            Self::ConfirmGeneration => "ConfirmGeneration",
            Self::CancelGeneration => "CancelGeneration",
            Self::Current => "Current",
            Self::DiagnoseCurrent => "DiagnoseCurrent",
            Self::AskCompare => "AskCompare",
            Self::EditOfficial(_) => "EditOfficial([hidden])",
            Self::PasteOfficial(_) => "PasteOfficial([hidden])",
            Self::BackspaceOfficial => "BackspaceOfficial",
            Self::ClearOfficial => "ClearOfficial",
            Self::CompareOfficial => "CompareOfficial",
            Self::CancelCompare => "CancelCompare",
            Self::Create => "Create",
            Self::AskEnd => "AskEnd",
            Self::CancelEnd => "CancelEnd",
            Self::ConfirmEnd => "ConfirmEnd",
            Self::Close => "Close",
            Self::Manage => "Manage",
            Self::AskJoin => "AskJoin",
            Self::CancelJoin => "CancelJoin",
            Self::RequestListenHere => "RequestListenHere",
            Self::ConfirmJoin => "ConfirmJoin",
            Self::EditJoin(_) => "EditJoin([private invitation])",
            Self::OpenInvitation => "OpenInvitation",
            Self::CloseInvitation => "CloseInvitation",
            Self::ExpandQr => "ExpandQr",
            Self::CloseQr => "CloseQr",
            Self::CopyInvitation => "CopyInvitation",
        })
    }
}

// RAM-only input, with no editor state, history, serialization or Debug.
#[derive(Default)]
pub struct OfficialDraft {
    input: String,
    pub invalid: bool,
}
impl OfficialDraft {
    pub fn has_input(&self) -> bool {
        !self.input.is_empty()
    }
    pub(crate) fn append(&mut self, value: String) {
        if self.input.len().saturating_add(value.len()) > 2048 {
            self.input = String::new();
            self.invalid = true;
        } else {
            if self.input.is_empty() {
                self.input = value;
            } else {
                self.input.push_str(&value);
            }
            self.invalid = false;
        }
    }
    pub(crate) fn backspace(&mut self) {
        self.input.pop();
        self.invalid = false;
    }
    pub(crate) fn clear(&mut self) {
        self.input = String::new();
        self.invalid = false;
    }
    pub(crate) fn take_link(&mut self) -> Result<crate::backend::jam::Link, Error> {
        let raw = std::mem::take(&mut self.input);
        let link = crate::backend::jam::Link::parse(&raw);
        self.invalid = link.is_err();
        link
    }
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
    pub generation: Option<GenerationView>,
    pub generation_ready: bool,
    // Surface state only: never preserves input or authority after retirement.
    pub official_open: bool,
    pub official: Option<OfficialDraft>,
    pub diagnostic: Option<crate::backend::jam::diagnostic::Diagnostic>,
    pub metadata: crate::backend::jam::metadata::Metadata,
    pub invitation_open: bool,
    pub qr_expanded: bool,
    pub qr: Option<qrcodegen::QrCode>,
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

/// Local confirmation/report only. No private URI, URL or auth is held here.
pub struct GenerationView {
    pub expected: Stamp,
    pub until: tokio::time::Instant,
    pub sent: bool,
    pub report: Option<crate::backend::jam::generation::Terminal>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: None,
            generation_ready: false,
            official_open: false,
            official: None,
            diagnostic: None,
            metadata: Default::default(),
            invitation_open: false,
            qr_expanded: false,
            qr: None,
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
            let generation = self.generation.take().filter(|view| {
                view.sent
                    && view.expected.account == live.account
                    && view.expected.engine == live.engine
                    && view.expected.revision == live.revision
                    && live.operation == view.expected.operation.wrapping_add(1)
            });
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
                self.clear_metadata();
                self.generation = generation;
                return;
            }
            let notice = self
                .confirmation
                .is_some()
                .then_some("The Jam changed. Check its state before continuing.");
            let official_open = self.official_open
                && self.stamp.account == live.account
                && self.stamp.engine == live.engine;
            *self = Self {
                generation,
                official_open,
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
        self.clear_metadata();
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
        if self.fresh() && matches!(event.snapshot.membership, Membership::Present(_)) {
            self.metadata = event.snapshot.metadata.clone();
        }
        true
    }

    pub fn clear_metadata(&mut self) {
        self.official = None;
        self.metadata = Default::default();
        self.close_invitation();
    }

    pub fn close_invitation(&mut self) {
        self.invitation_open = false;
        self.qr_expanded = false;
        self.qr = None;
    }

    pub fn invitation(&self) -> Option<&str> {
        self.owner_id()?;
        self.metadata.invitation.as_ref().map(|link| link.url())
    }

    pub fn host_name(&self) -> Option<&str> {
        if !self.fresh() {
            return None;
        }
        let owner = self.metadata.owner.as_deref()?;
        let member = self
            .metadata
            .members
            .as_ref()?
            .iter()
            .find(|m| m.named_by(owner))?;
        member.name.as_deref()
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
        self.official_open = false;
        self.engaged = true;
        self.pending = Some(kind);
        self.confirmation = None;
        self.error = None;
        self.notice = None;
        self.join = None;
        self.clear_metadata();
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
    #[test]
    fn unknown_and_old_operation_metadata_never_replenish_authority() {
        use crate::backend::jam::metadata::{Invitation, Metadata};
        let mut state = State::default();
        let live = state.stamp;
        let mut snapshot = event(live, owner());
        snapshot.snapshot.metadata = Metadata {
            invitation: Invitation::parse("https://open.spotify.com/socialsession/Synthetic08"),
            ..Default::default()
        };
        state.observe(&snapshot, live);
        assert!(state.invitation().is_some());
        let deadline = state.fresh_until;
        let mut old = event(
            Stamp {
                operation: 99,
                ..live
            },
            owner(),
        );
        old.snapshot.metadata = snapshot.snapshot.metadata.clone();
        assert!(!state.observe(&old, live));
        assert_eq!(state.fresh_until, deadline);
        state.invitation_open = true;
        state.qr = Some(
            qrcodegen::QrCode::encode_text(
                state.invitation().unwrap(),
                qrcodegen::QrCodeEcc::Medium,
            )
            .unwrap(),
        );
        let mut unknown = event(live, Membership::Unknown);
        unknown.snapshot.fresh_until = None;
        unknown.snapshot.reconcile_required = true;
        state.observe(&unknown, live);
        assert!(state.metadata.invitation.is_none() && state.qr.is_none());
        assert!(state.owner_id().is_none());
    }
    fn event(stamp: Stamp, membership: Membership) -> Event {
        Event {
            stamp,
            phase: Phase::Complete(Kind::Current),
            snapshot: Snapshot {
                metadata: Default::default(),
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

#[cfg(test)]
mod official_tests {
    use super::*;
    #[test]
    fn official_input_is_strict_hidden_and_one_use_even_when_actions_are_cloned() {
        for value in [
            "Case9",
            "https://spotify.link/private",
            "spotify://sessions/join/Case9",
            " https://open.spotify.com/socialsession/Case9",
            "http://open.spotify.com/socialsession/Case9",
            "https://open.spotify.com/socialsession/Case9?bad=x",
        ] {
            let mut draft = OfficialDraft::default();
            draft.append(value.into());
            assert!(draft.take_link().is_err());
            assert!(!draft.has_input() && draft.invalid);
        }
        let edit =
            OfficialEdit::new("https://open.spotify.com/socialsession/SyntheticSecret9".into());
        let cloned = edit.clone();
        let mut draft = OfficialDraft::default();
        draft.append(edit.take().unwrap());
        assert!(cloned.take().is_none());
        let action = Intent::EditOfficial(cloned);
        assert!(!format!("{action:?}").contains("SyntheticSecret9"));
        assert!(draft.take_link().is_ok() && !draft.has_input());
        draft.append("x".repeat(2049));
        assert!(!draft.has_input() && draft.invalid);
        draft.append("secret".into());
        draft.clear();
        assert!(!draft.has_input());
    }
    #[test]
    fn official_surface_is_non_authoritative_and_closes_across_account_or_engine() {
        for live in [
            Stamp {
                revision: 1,
                ..Default::default()
            },
            Stamp {
                account: 1,
                ..Default::default()
            },
            Stamp {
                engine: 1,
                ..Default::default()
            },
        ] {
            let mut state = State {
                official_open: true,
                official: Some(OfficialDraft::default()),
                ..Default::default()
            };
            state.official.as_mut().unwrap().append("Synthetic9".into());
            state.sync(live);
            assert!(state.official.is_none() && state.owner_id().is_none());
            assert_eq!(state.official_open, live.account == 0 && live.engine == 0);
        }
        let edit = OfficialEdit::new("Synthetic9".into());
        let action = Intent::PasteOfficial(edit.clone());
        assert!(!format!("{action:?}").contains("Synthetic9"));
        assert!(edit.take().is_some());
        let Intent::PasteOfficial(cloned) = action else {
            unreachable!()
        };
        assert!(cloned.take().is_none());
    }

    #[test]
    fn official_draft_never_survives_context_retirement_or_pending() {
        let mut state = State {
            official: Some(OfficialDraft::default()),
            ..Default::default()
        };
        state.official.as_mut().unwrap().append("synthetic".into());
        state.sync(Stamp {
            account: 1,
            ..Default::default()
        });
        assert!(state.official.is_none());
        state.official = Some(OfficialDraft::default());
        state.start(Kind::Current);
        assert!(state.official.is_none());
        state.official = Some(OfficialDraft::default());
        state.clear_metadata();
        assert!(state.official.is_none());
    }
}
