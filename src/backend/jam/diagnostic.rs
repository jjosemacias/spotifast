//! One opt-in Current observation. No identity, raw response or persistent output.
pub use super::comparison::{Comparison, OfficialRelation, Shape};
use super::lifecycle::Cause;
use super::{Arc, DEADLINE, Error, Instant, Link, Membership, Mutex, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Queued,
    Preflight,
    Budget,
    Preparation,
    Transport,
    Body,
    Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schema {
    None,
    BodyLimit,
    Mime,
    JsonStructure,
    Metadata,
    Projection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlField {
    NotObserved,
    Absent,
    Blank,
    Accepted,
    FormatRejected,
    TypeRejected,
    AliasConflict,
    BoundsOrControlRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatBoundary {
    NotApplicable,
    PrefixOpenOtherPath,
    PrefixSpotifyOther,
    PrefixHttpsOther,
    PrefixStructured,
    PrefixOther,
    TokenEmptyOrLimit,
    TokenPlaceholderMarker,
    TokenQuery,
    TokenFragment,
    TokenPercentMarker,
    TokenOtherCharacters,
    QueryShape,
    QueryKeyOrDuplicate,
    QueryValueRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Waiting(Stage),
    Failed(Stage, Error),
    Absent,
    Present,
    Invalidated(Cause),
}

/// Fixed classifications for one deliberate Current, never field values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub class: Class,
    pub http: Option<u16>,
    pub schema: Schema,
    pub url_field: UrlField,
    pub format_boundary: FormatBoundary,
    pub comparison: Option<Comparison>,
}

// No Debug/Clone/Serialize: input and initial Jam ID are operation-local only.
struct OfficialComparison {
    official: Option<Link>,
    host: Option<String>,
    host_until: Option<Instant>,
    deadline: Instant,
    pending: Option<Comparison>,
    verified_host: bool,
}

struct Slot {
    comparison: Option<OfficialComparison>,
    value: Diagnostic,
    closed: bool,
    waker: crate::backend::Waker,
}

/// A fresh handle per deliberate diagnostic intention; old tasks cannot write
/// to a replacement slot. Retirement seals it before cancellation.
#[derive(Clone)]
pub struct Probe(Arc<Mutex<Slot>>);

impl Default for Probe {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Slot {
            comparison: None,
            value: Diagnostic {
                class: Class::Waiting(Stage::Queued),
                http: None,
                schema: Schema::None,
                url_field: UrlField::NotObserved,
                format_boundary: FormatBoundary::NotApplicable,
                comparison: None,
            },
            closed: false,
            waker: Default::default(),
        })))
    }
}

impl Probe {
    /// Takes only a strict long-link token; never stores the raw GUI input.
    pub(crate) fn with_official(official: Link) -> Self {
        let probe = Self::default();
        {
            let mut slot = probe
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.value.comparison = Some(Comparison::default());
            slot.comparison = Some(OfficialComparison {
                official: Some(official),
                host: None,
                host_until: None,
                deadline: Instant::now() + DEADLINE,
                pending: None,
                verified_host: false,
            });
        }
        probe
    }
    pub(super) fn is_comparison(&self) -> bool {
        self.snapshot().comparison.is_some()
    }
    pub(super) fn bind_host(&self, snapshot: &Snapshot) -> Result<(), Error> {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.closed {
            return Err(Error::Stale);
        }
        let comparison = slot.comparison.as_mut().ok_or(Error::Stale)?;
        if comparison.host.is_some() || comparison.official.is_none() {
            return Err(Error::Stale);
        }
        if Instant::now() >= comparison.deadline {
            return Err(Error::Timeout);
        }
        if snapshot.reconcile_required || snapshot.error.is_some() {
            return Err(Error::ReconcileRequired);
        }
        let until = snapshot
            .fresh_until
            .filter(|until| Instant::now() < *until)
            .ok_or(Error::SnapshotExpired)?;
        let Membership::Present(host) = &snapshot.membership else {
            return Err(Error::NotOwner);
        };
        if !host.is_session_owner {
            return Err(Error::NotOwner);
        }
        comparison.host = Some(host.session_id.clone());
        comparison.host_until = Some(until);
        Ok(())
    }
    pub(super) fn comparison_deadline(&self) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .comparison
            .as_ref()
            .map(|c| c.deadline)
    }
    pub(super) fn comparison_fresh_until(&self) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .comparison
            .as_ref()
            .and_then(|c| c.host_until.map(|until| until.min(c.deadline)))
    }
    pub(super) fn check_comparison(&self) -> Result<(), Error> {
        let slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.value.comparison.is_none() {
            return Ok(());
        }
        if slot.closed {
            return Err(Error::Stale);
        }
        let c = slot.comparison.as_ref().ok_or(Error::Stale)?;
        if c.host.is_none() {
            return Err(Error::NotReady);
        }
        if Instant::now() >= c.deadline {
            return Err(Error::Timeout);
        }
        Ok(())
    }
    pub(super) fn compare_url(&self, value: &str) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.closed {
            return;
        }
        if let Some(c) = &mut slot.comparison
            && let Some(official) = c.official.take()
        {
            c.pending = Some(super::comparison::compare(value, &official));
        }
    }
    /// Only Bridge's live completion gate can expose the provisional enums.
    pub(super) fn commit_comparison(&self, snapshot: &Snapshot) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(c) = slot.comparison.take() else {
            return;
        };
        if !slot.closed
            && c.verified_host
            && Instant::now() < c.deadline
            && snapshot.error.is_none()
            && !snapshot.reconcile_required
            && snapshot
                .fresh_until
                .is_some_and(|until| Instant::now() < until)
            && matches!(&snapshot.membership, Membership::Present(host) if host.is_session_owner)
        {
            slot.value.comparison = Some(c.pending.unwrap_or_default());
            slot.waker.wake();
        }
    }
    fn update(&self, apply: impl FnOnce(&mut Diagnostic)) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot.closed {
            apply(&mut slot.value);
            slot.waker.wake();
        }
    }
    pub(super) fn wake_with(&self, waker: crate::backend::Waker) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waker = waker;
    }
    pub fn snapshot(&self) -> Diagnostic {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .comparison
            .as_ref()
            .is_some_and(|c| Instant::now() >= c.deadline)
        {
            slot.comparison = None;
            slot.value.comparison = Some(Comparison::default());
            let stage = match slot.value.class {
                Class::Waiting(stage) | Class::Failed(stage, _) => stage,
                _ => Stage::Response,
            };
            slot.value.class = Class::Failed(stage, Error::Timeout);
            slot.closed = true;
        }
        slot.value
    }
    pub(super) fn stage(&self, stage: Stage) {
        self.update(|d| d.class = Class::Waiting(stage));
    }
    pub(super) fn status(&self, status: u16) {
        self.update(|d| d.http = Some(status));
    }
    pub(super) fn schema(&self, schema: Schema) {
        self.update(|d| d.schema = schema);
    }
    pub(super) fn url_field(&self, value: UrlField) {
        self.update(|d| {
            d.url_field = value;
            d.format_boundary = FormatBoundary::NotApplicable;
        });
    }
    pub(super) fn invitation_format(&self, result: Result<(), FormatBoundary>) {
        self.update(|d| {
            d.url_field = if result.is_ok() {
                UrlField::Accepted
            } else {
                UrlField::FormatRejected
            };
            d.format_boundary = result.err().unwrap_or(FormatBoundary::NotApplicable);
        });
    }
    pub(super) fn finish(&self, result: Result<&Membership, Error>) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.closed {
            slot.comparison = None;
            return;
        }
        slot.value.class = match result {
            Ok(Membership::Absent) => Class::Absent,
            Ok(Membership::Present(_)) => Class::Present,
            Ok(Membership::Unknown) => Class::Failed(Stage::Response, Error::VerificationFailed),
            Err(error) => {
                let stage = match slot.value.class {
                    Class::Waiting(stage) | Class::Failed(stage, _) => stage,
                    _ => Stage::Preflight,
                };
                Class::Failed(stage, error)
            }
        };
        if let Some(mut c) = slot.comparison.take() {
            c.official = None;
            let host = c.host.take();
            c.verified_host = matches!(result, Ok(Membership::Present(p)) if p.is_session_owner && host.as_deref() == Some(p.session_id.as_str()));
            if c.verified_host && Instant::now() < c.deadline {
                slot.comparison = Some(c);
            }
        }
        slot.waker.wake();
    }
    pub(super) fn retire(&self, cause: Cause) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = slot.value;
        slot.comparison = None;
        if slot.value.comparison.is_some() {
            slot.value.comparison = Some(Comparison::default());
        }
        // Context loss sanitizes even an already retired record. Never carry
        // status/schema across an unverified account or replacement engine.
        if matches!(cause, Cause::Account | Cause::Engine | Cause::Context) {
            slot.value.http = None;
            slot.value.schema = Schema::None;
            slot.value.url_field = UrlField::NotObserved;
            slot.value.format_boundary = FormatBoundary::NotApplicable;
        } else if slot.closed {
            return;
        }
        slot.value.class = Class::Invalidated(cause);
        slot.closed = true;
        if slot.value != before {
            slot.waker.wake();
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("class=")?;
        match self.class {
            Class::Waiting(stage) => write!(f, "{}_waiting", stage.label())?,
            Class::Failed(stage, error) => write!(
                f,
                "{}_{}",
                stage.label(),
                match error {
                    Error::Busy => "busy",
                    Error::Stale => "stale",
                    Error::NotReady => "not_ready",
                    Error::Transport => "transport",
                    Error::Timeout => "timeout",
                    Error::InvalidResponse => "invalid_response",
                    Error::Http(_) => "http",
                    _ => "other_refusal",
                }
            )?,
            Class::Absent => f.write_str("current_absent")?,
            Class::Present => f.write_str("current_present")?,
            Class::Invalidated(cause) => f.write_str(match cause {
                Cause::Account => "invalidated_account",
                Cause::Engine => "invalidated_engine",
                Cause::Context => "invalidated_context",
                Cause::Connection => "invalidated_connection",
                Cause::Jam => "invalidated_jam",
                Cause::ListenerClosed => "invalidated_listener",
            })?,
        }
        match self.http {
            Some(status) => write!(f, "; http={status}")?,
            None => f.write_str("; http=none")?,
        }
        write!(
            f,
            "; schema={}; source=deliberate_current; url_field={}; format_boundary={}",
            match self.schema {
                Schema::None => "none",
                Schema::BodyLimit => "body_limit",
                Schema::Mime => "mime",
                Schema::JsonStructure => "json_structure",
                Schema::Metadata => "metadata",
                Schema::Projection => "projection",
            },
            self.url_field.label(),
            self.format_boundary.label()
        )?;
        if let Some(comparison) = self.comparison {
            write!(
                f,
                "; shape={}; official_relation={}",
                comparison.shape.label(),
                comparison.official_relation.label()
            )?;
        }
        Ok(())
    }
}

impl FormatBoundary {
    fn label(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::PrefixOpenOtherPath => "prefix_open_other_path",
            Self::PrefixSpotifyOther => "prefix_spotify_other",
            Self::PrefixHttpsOther => "prefix_https_other",
            Self::PrefixStructured => "prefix_structured",
            Self::PrefixOther => "prefix_other",
            Self::TokenEmptyOrLimit => "token_empty_or_limit",
            Self::TokenPlaceholderMarker => "token_placeholder_marker",
            Self::TokenQuery => "token_query",
            Self::TokenFragment => "token_fragment",
            Self::TokenPercentMarker => "token_percent_marker",
            Self::TokenOtherCharacters => "token_other_characters",
            Self::QueryShape => "query_shape",
            Self::QueryKeyOrDuplicate => "query_key_or_duplicate",
            Self::QueryValueRejected => "query_value_rejected",
        }
    }
}

impl UrlField {
    fn label(self) -> &'static str {
        match self {
            Self::NotObserved => "not_observed",
            Self::Absent => "absent",
            Self::Blank => "blank",
            Self::Accepted => "accepted",
            Self::FormatRejected => "format_rejected",
            Self::TypeRejected => "type_rejected",
            Self::AliasConflict => "alias_conflict",
            Self::BoundsOrControlRejected => "bounds_or_control_rejected",
        }
    }
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queue",
            Self::Preflight => "preflight",
            Self::Budget => "budget",
            Self::Preparation => "preparation",
            Self::Transport => "transport",
            Self::Body => "body",
            Self::Response => "response",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_bounded_to_fixed_fields_and_retirement_seals_all_updates() {
        let stages = [
            Stage::Queued,
            Stage::Preflight,
            Stage::Budget,
            Stage::Preparation,
            Stage::Transport,
            Stage::Body,
            Stage::Response,
        ];
        let errors = [
            Error::Busy,
            Error::Stale,
            Error::NotReady,
            Error::Transport,
            Error::Timeout,
            Error::InvalidResponse,
            Error::Http(405),
            Error::InvalidLink,
        ];
        let fields = [
            UrlField::NotObserved,
            UrlField::Absent,
            UrlField::Blank,
            UrlField::Accepted,
            UrlField::FormatRejected,
            UrlField::TypeRejected,
            UrlField::AliasConflict,
            UrlField::BoundsOrControlRejected,
        ];
        let boundaries = [
            FormatBoundary::NotApplicable,
            FormatBoundary::PrefixOpenOtherPath,
            FormatBoundary::PrefixSpotifyOther,
            FormatBoundary::PrefixHttpsOther,
            FormatBoundary::PrefixStructured,
            FormatBoundary::PrefixOther,
            FormatBoundary::TokenEmptyOrLimit,
            FormatBoundary::TokenPlaceholderMarker,
            FormatBoundary::TokenQuery,
            FormatBoundary::TokenFragment,
            FormatBoundary::TokenPercentMarker,
            FormatBoundary::TokenOtherCharacters,
            FormatBoundary::QueryShape,
            FormatBoundary::QueryKeyOrDuplicate,
            FormatBoundary::QueryValueRejected,
        ];
        for stage in stages {
            for error in errors {
                for url_field in fields {
                    for format_boundary in boundaries {
                        let d = Diagnostic {
                            class: Class::Failed(stage, error),
                            http: Some(405),
                            schema: Schema::None,
                            url_field,
                            format_boundary,
                            comparison: None,
                        };
                        let text = d.to_string();
                        assert!(text.len() < 256);
                        assert_eq!(text.split(';').count(), 6);
                        assert_eq!(
                            text.split(';')
                                .map(|field| field.trim().split_once('=').unwrap().0)
                                .collect::<Vec<_>>(),
                            [
                                "class",
                                "http",
                                "schema",
                                "source",
                                "url_field",
                                "format_boundary"
                            ]
                        );
                        assert!(text.contains("; source=deliberate_current; url_field="));
                        assert!(text.ends_with(format_boundary.label()));
                        assert!(text.bytes().all(|b| b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || b"_=; ".contains(&b)));
                    }
                }
            }
        }
        let probe = Probe::default();
        probe.status(200);
        probe.schema(Schema::Metadata);
        probe.invitation_format(Err(FormatBoundary::TokenQuery));
        probe.retire(Cause::Connection);
        let retired = probe.snapshot();
        probe.stage(Stage::Transport);
        probe.status(404);
        probe.schema(Schema::None);
        probe.url_field(UrlField::FormatRejected);
        probe.invitation_format(Err(FormatBoundary::PrefixOther));
        probe.finish(Ok(&Membership::Absent));
        probe.retire(Cause::Jam);
        assert_eq!(probe.snapshot(), retired);
        probe.retire(Cause::Context);
        assert_eq!(probe.snapshot().http, None);
        assert_eq!(probe.snapshot().schema, Schema::None);
        assert_eq!(probe.snapshot().url_field, UrlField::NotObserved);
        assert_eq!(
            probe.snapshot().format_boundary,
            FormatBoundary::NotApplicable
        );
        assert_eq!(probe.snapshot().class, Class::Invalidated(Cause::Context));
    }

    #[test]
    fn account_engine_and_context_retirement_clear_the_url_category_and_seal_late_updates() {
        for cause in [Cause::Account, Cause::Engine, Cause::Context] {
            let probe = Probe::default();
            probe.invitation_format(Err(FormatBoundary::TokenQuery));
            probe.retire(cause);
            probe.invitation_format(Err(FormatBoundary::TokenFragment));
            assert_eq!(probe.snapshot().url_field, UrlField::NotObserved);
            assert_eq!(
                probe.snapshot().format_boundary,
                FormatBoundary::NotApplicable
            );
            assert_eq!(probe.snapshot().class, Class::Invalidated(cause));
        }
    }

    #[test]
    fn a_new_field_result_clears_an_earlier_boundary_in_the_same_slot() {
        let probe = Probe::default();
        for field in [
            UrlField::Absent,
            UrlField::Blank,
            UrlField::TypeRejected,
            UrlField::AliasConflict,
            UrlField::BoundsOrControlRejected,
        ] {
            probe.invitation_format(Err(FormatBoundary::TokenQuery));
            probe.url_field(field);
            assert_eq!(
                probe.snapshot().format_boundary,
                FormatBoundary::NotApplicable
            );
        }
        probe.invitation_format(Err(FormatBoundary::TokenQuery));
        probe.invitation_format(Ok(()));
        assert_eq!(probe.snapshot().url_field, UrlField::Accepted);
        assert_eq!(
            probe.snapshot().format_boundary,
            FormatBoundary::NotApplicable
        );
    }
}

#[cfg(test)]
mod official_tests {
    use super::*;
    use crate::backend::jam::{FRESH_FOR, Projection};
    fn host(id: &str) -> Snapshot {
        Snapshot {
            metadata: Default::default(),
            membership: Membership::Present(Projection {
                session_id: id.into(),
                is_session_owner: true,
                is_listening: false,
                is_controlling: false,
            }),
            fresh_until: Some(Instant::now() + FRESH_FOR),
            reconcile_required: false,
            error: None,
        }
    }
    fn probe(snapshot: &Snapshot) -> Probe {
        let p = Probe::with_official(
            Link::parse("https://open.spotify.com/socialsession/SyntheticSecret9").unwrap(),
        );
        p.bind_host(snapshot).unwrap();
        p
    }
    #[tokio::test(start_paused = true)]
    async fn official_secret_is_consumed_once_and_result_waits_for_live_delivery() {
        let h = host("synthetic-jam");
        let p = probe(&h);
        p.compare_url("SyntheticSecret9");
        p.compare_url("Different9");
        assert!(
            p.0.lock()
                .unwrap()
                .comparison
                .as_ref()
                .unwrap()
                .official
                .is_none()
        );
        assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
        p.finish(Ok(&h.membership));
        assert!(
            p.0.lock()
                .unwrap()
                .comparison
                .as_ref()
                .unwrap()
                .host
                .is_none()
        );
        assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
        p.commit_comparison(&h);
        assert!(p.0.lock().unwrap().comparison.is_none());
        assert_eq!(
            p.snapshot().comparison.unwrap().official_relation,
            OfficialRelation::Same
        );
        let output = p.snapshot().to_string();
        assert!(!output.contains("SyntheticSecret9") && !output.contains("synthetic-jam"));
        assert_eq!(output.split(';').count(), 8);
        assert!(output.len() < 384);
    }
    #[tokio::test(start_paused = true)]
    async fn official_retirement_purges_secret_and_seals_every_late_path() {
        for cause in [
            Cause::Account,
            Cause::Engine,
            Cause::Context,
            Cause::Connection,
            Cause::Jam,
            Cause::ListenerClosed,
        ] {
            for compared in [false, true] {
                let h = host("synthetic-jam");
                let p = probe(&h);
                if compared {
                    p.compare_url("SyntheticSecret9");
                    p.finish(Ok(&h.membership));
                }
                p.retire(cause);
                assert!(p.0.lock().unwrap().comparison.is_none());
                p.compare_url("SyntheticSecret9");
                p.finish(Ok(&h.membership));
                p.commit_comparison(&h);
                assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
                assert!(p.check_comparison().is_err());
            }
        }
    }
    #[tokio::test(start_paused = true)]
    async fn official_schema_role_identity_and_deadline_errors_never_publish_a_match() {
        for case in 0..7 {
            let h = host("synthetic-jam");
            let p = probe(&h);
            p.compare_url("SyntheticSecret9");
            let mut result = h.clone();
            match case {
                0 => p.finish(Err(Error::InvalidResponse)),
                1 => p.finish(Err(Error::Transport)),
                2 => {
                    result.membership = Membership::Absent;
                    p.finish(Ok(&result.membership));
                }
                3 => {
                    if let Membership::Present(m) = &mut result.membership {
                        m.is_session_owner = false;
                    }
                    p.finish(Ok(&result.membership));
                }
                4 => {
                    result.membership = host("other-jam").membership;
                    p.finish(Ok(&result.membership));
                }
                _ => {
                    p.finish(Ok(&h.membership));
                    tokio::time::advance(std::time::Duration::from_secs(if case == 5 {
                        21
                    } else {
                        16
                    }))
                    .await;
                }
            }
            p.commit_comparison(&result);
            assert_eq!(
                p.snapshot().comparison,
                Some(Comparison::default()),
                "case {case}"
            );
            assert!(p.0.lock().unwrap().comparison.is_none());
        }
    }
    #[tokio::test]
    async fn official_metadata_does_not_fallback_to_token_or_uri_and_full_schema_still_wins() {
        let h = host("synthetic-jam");
        for (url, extra) in [("Different9", false), ("SyntheticSecret9", true)] {
            let p = probe(&h);
            let mut root = serde_json::json!({"session_id":"synthetic-jam","is_session_owner":true,"is_listening":false,"is_controlling":false,
                "join_session_url":url,"join_session_token":"SyntheticSecret9","join_session_uri":"spotify://sessions/join/SyntheticSecret9"});
            if extra {
                root.as_object_mut().unwrap().remove("is_listening");
            }
            let reply = crate::backend::jam::Reply {
                status: 200,
                content_type: None,
                body: serde_json::to_vec(&root).unwrap(),
            };
            let read = crate::backend::jam::read_reply_observed(
                reply,
                &crate::backend::jam::Profile::ObservedFourFields,
                Some(&p),
            );
            p.finish(read.as_ref().map(|r| &r.membership).map_err(|e| *e));
            p.commit_comparison(&h);
            assert_eq!(
                p.snapshot().comparison.unwrap().official_relation,
                if extra {
                    OfficialRelation::NotComparable
                } else {
                    OfficialRelation::Different
                }
            );
            assert!(p.0.lock().unwrap().comparison.is_none());
        }
    }
}

#[cfg(test)]
mod official_error_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn official_queued_expiry_and_alias_type_failures_discard_unconsumed_input() {
        let p = Probe::with_official(
            Link::parse("https://open.spotify.com/socialsession/SyntheticSecret9").unwrap(),
        );
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
        assert!(p.0.lock().unwrap().comparison.is_none());
        for fields in [
            serde_json::json!({"join_session_url":true}),
            serde_json::json!({"join_session_url":"SyntheticSecret9","joinSessionUrl":"SyntheticSecret9"}),
            serde_json::json!({"join_session_url":"secret\n"}),
        ] {
            let p = Probe::with_official(
                Link::parse("https://open.spotify.com/socialsession/SyntheticSecret9").unwrap(),
            );
            let mut root = serde_json::json!({"session_id":"synthetic","is_session_owner":true,"is_listening":false,"is_controlling":false});
            root.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let reply = crate::backend::jam::Reply {
                status: 200,
                content_type: None,
                body: serde_json::to_vec(&root).unwrap(),
            };
            let result = crate::backend::jam::read_reply_observed(
                reply,
                &crate::backend::jam::Profile::ObservedFourFields,
                Some(&p),
            );
            assert!(result.is_err());
            p.finish(Err(Error::InvalidResponse));
            assert!(p.0.lock().unwrap().comparison.is_none());
            assert_eq!(p.snapshot().comparison, Some(Comparison::default()));
        }
    }
}
