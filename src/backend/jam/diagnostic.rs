//! One opt-in Current observation. No identity, raw response or persistent output.
use super::lifecycle::Cause;
use super::{Arc, Error, Membership, Mutex};

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
pub enum Class {
    Waiting(Stage),
    Failed(Stage, Error),
    Absent,
    Present,
    Invalidated(Cause),
}

/// These are the only three fields exposed to the presenter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub class: Class,
    pub http: Option<u16>,
    pub schema: Schema,
}

struct Slot {
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
            value: Diagnostic {
                class: Class::Waiting(Stage::Queued),
                http: None,
                schema: Schema::None,
            },
            closed: false,
            waker: Default::default(),
        })))
    }
}

impl Probe {
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
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .value
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
    pub(super) fn finish(&self, result: Result<&Membership, Error>) {
        self.update(|d| {
            d.class = match result {
                Ok(Membership::Absent) => Class::Absent,
                Ok(Membership::Present(_)) => Class::Present,
                Ok(Membership::Unknown) => {
                    Class::Failed(Stage::Response, Error::VerificationFailed)
                }
                Err(error) => {
                    let stage = match d.class {
                        Class::Waiting(stage) | Class::Failed(stage, _) => stage,
                        _ => Stage::Preflight,
                    };
                    Class::Failed(stage, error)
                }
            };
        });
    }
    pub(super) fn retire(&self, cause: Cause) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = slot.value;
        // Context loss sanitizes even an already retired record. Never carry
        // status/schema across an unverified account or replacement engine.
        if matches!(cause, Cause::Account | Cause::Engine | Cause::Context) {
            slot.value.http = None;
            slot.value.schema = Schema::None;
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
            "; schema={}",
            match self.schema {
                Schema::None => "none",
                Schema::BodyLimit => "body_limit",
                Schema::Mime => "mime",
                Schema::JsonStructure => "json_structure",
                Schema::Metadata => "metadata",
                Schema::Projection => "projection",
            }
        )
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
    fn output_is_bounded_to_three_fixed_fields_and_retirement_seals_all_updates() {
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
        for stage in stages {
            for error in errors {
                let d = Diagnostic {
                    class: Class::Failed(stage, error),
                    http: Some(405),
                    schema: Schema::None,
                };
                let text = d.to_string();
                assert!(text.len() < 128);
                assert_eq!(text.split(';').count(), 3);
                assert!(
                    text.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"_=; ".contains(&b))
                );
            }
        }
        let probe = Probe::default();
        probe.status(200);
        probe.schema(Schema::Metadata);
        probe.retire(Cause::Connection);
        let retired = probe.snapshot();
        probe.stage(Stage::Transport);
        probe.status(404);
        probe.schema(Schema::None);
        probe.finish(Ok(&Membership::Absent));
        probe.retire(Cause::Jam);
        assert_eq!(probe.snapshot(), retired);
        probe.retire(Cause::Context);
        assert_eq!(probe.snapshot().http, None);
        assert_eq!(probe.snapshot().schema, Schema::None);
        assert_eq!(probe.snapshot().class, Class::Invalidated(Cause::Context));
    }
}
