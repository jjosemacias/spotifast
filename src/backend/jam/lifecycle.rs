//! Backend binding, passive Dealer invalidation and deliberate Jam requests.
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Semaphore, watch};
use tokio::task::AbortHandle;

use super::*;
use crate::backend::{Command, Waker};

/// Reserve local subscriptions while the Session still owns its Dealer builder,
/// before Spirc's event loop can take it to launch the connection. The streams
/// travel with that same Session; accepting an engine must not subscribe again.
pub(crate) struct PreparedListeners {
    session: Session,
    jam: Result<librespot_core::dealer::manager::BoxedStreamResult<()>, librespot_core::Error>,
    connections: Result<
        librespot_core::dealer::manager::BoxedStreamResult<Option<String>>,
        librespot_core::Error,
    >,
}

impl PreparedListeners {
    pub(crate) fn new(session: Session) -> Self {
        let jam = session
            .dealer()
            .listen_for("social-connect/v2/session_update", |message| {
                if let librespot_core::dealer::protocol::PayloadValue::Json(json) = &message.payload
                {
                    // A decoded delta still cannot establish membership or ordering.
                    let _ = metadata::dealer(json.as_bytes());
                }
                Ok(())
            });
        let connections = session
            .dealer()
            .listen_for("hm://pusher/v1/connections/", |message| {
                Ok(message.headers.get("Spotify-Connection-Id").cloned())
            });
        Self {
            session,
            jam,
            connections,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub account: u64,
    pub engine: u64,
    pub revision: u64,
    pub operation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Current,
    Create,
    Join,
    End,
}

pub enum Action {
    Current,
    DiagnoseCurrent(Probe),
    Create(CreateIntent),
    Join {
        link: Link,
        intent: JoinIntent,
    },
    End {
        session_id: String,
        intent: EndIntent,
    },
}

impl Action {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Current | Self::DiagnoseCurrent(_) => Kind::Current,
            Self::Create(_) => Kind::Create,
            Self::Join { .. } => Kind::Join,
            Self::End { .. } => Kind::End,
        }
    }
}

/// Echo the stamp of the observed backend state. Old queued intentions are refused.
pub struct Request {
    pub expected: Stamp,
    pub action: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Account,
    Engine,
    Jam,
    Connection,
    ListenerClosed,
    Context,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Pending(Kind),
    Complete(Kind),
    Invalidated(Cause),
}

/// Coalesced backend state, not a log or raw Dealer response.
pub struct Event {
    pub stamp: Stamp,
    pub phase: Phase,
    pub snapshot: Snapshot,
}

type Work<'a> = Pin<Box<dyn Future<Output = Result<Membership, Error>> + Send + 'a>>;
type AckWork<'a> = Pin<Box<dyn Future<Output = Result<CreateAck, Error>> + Send + 'a>>;
type VerifyWork<'a> = Pin<Box<dyn Future<Output = Result<Snapshot, Error>> + Send + 'a>>;

pub(super) struct CreateLease {
    hard_epoch: watch::Receiver<u64>,
    pub(super) deadline: Instant,
    pub(super) emitted: AtomicBool,
    pub(super) verifier_emitted: AtomicBool,
    cancelled: AtomicBool,
    pub(super) verified: AtomicBool,
}
impl CreateLease {
    pub(super) fn new(hard_epoch: watch::Receiver<u64>) -> Self {
        Self {
            hard_epoch,
            deadline: Instant::now() + DEADLINE,
            emitted: AtomicBool::new(false),
            verifier_emitted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            verified: AtomicBool::new(false),
        }
    }

    pub(super) fn check(&self) -> Result<(), Error> {
        if self.cancelled.load(Ordering::SeqCst)
            || !matches!(self.hard_epoch.has_changed(), Ok(false))
        {
            return Err(Error::Stale);
        }
        if Instant::now() >= self.deadline {
            return Err(Error::Timeout);
        }
        Ok(())
    }
}
trait Host: Send + Sync {
    fn resume_intent(
        &self,
        authorization: &resume::Authorization,
        paused: Arc<dyn Fn() -> bool + Send + Sync>,
        gesture: Instant,
    ) -> Result<(resume::Intent, resume::Cancellation), resume::Terminal>;
    fn resume(
        &self,
        intent: resume::Intent,
        budget: Arc<Semaphore>,
    ) -> Pin<Box<dyn Future<Output = resume::Outcome> + Send + '_>>;
    fn purge_generation(&self);
    fn generation_until(&self) -> Result<Instant, generation::Terminal>;
    fn generate(
        &self,
        lease: Arc<generation::Lease>,
    ) -> Pin<Box<dyn Future<Output = generation::Terminal> + Send + '_>>;
    fn context(&self) -> Result<Context, Error>;
    fn invalidate(&self);
    fn snapshot(&self) -> Snapshot;
    fn run(&self, action: Action) -> Work<'_>;
    fn capture_create(&self, lease: Arc<CreateLease>) -> AckWork<'_>;
    fn verify_create(&self, ack: CreateAck, lease: Arc<CreateLease>) -> VerifyWork<'_>;
    fn commit_create(&self);
}

impl<T: resume::Wire> Host for Kernel<T> {
    fn resume_intent(
        &self,
        authorization: &resume::Authorization,
        paused: Arc<dyn Fn() -> bool + Send + Sync>,
        gesture: Instant,
    ) -> Result<(resume::Intent, resume::Cancellation), resume::Terminal> {
        self.experimental_resume_intent_at(authorization, paused, gesture)
    }
    fn resume(
        &self,
        intent: resume::Intent,
        budget: Arc<Semaphore>,
    ) -> Pin<Box<dyn Future<Output = resume::Outcome> + Send + '_>> {
        Box::pin(self.experimental_resume(intent, budget))
    }
    fn purge_generation(&self) {
        self.state().generation_uri = None;
    }
    fn generation_until(&self) -> Result<Instant, generation::Terminal> {
        self.generation_until()
    }
    fn generate(
        &self,
        lease: Arc<generation::Lease>,
    ) -> Pin<Box<dyn Future<Output = generation::Terminal> + Send + '_>> {
        Box::pin(self.generate(lease))
    }
    fn capture_create(&self, lease: Arc<CreateLease>) -> AckWork<'_> {
        Box::pin(self.capture_create(lease))
    }
    fn verify_create(&self, ack: CreateAck, lease: Arc<CreateLease>) -> VerifyWork<'_> {
        Box::pin(self.verify_create(ack, lease))
    }
    fn commit_create(&self) {
        self.commit_create();
    }
    fn context(&self) -> Result<Context, Error> {
        self.transport.context()
    }
    fn invalidate(&self) {
        self.invalidate();
    }
    fn snapshot(&self) -> Snapshot {
        self.snapshot()
    }
    fn run(&self, action: Action) -> Work<'_> {
        Box::pin(async move {
            match action {
                Action::Current => self.current().await,
                Action::DiagnoseCurrent(probe) => {
                    self.run_observed(Operation::Current, Some(&probe)).await
                }
                // No generic Create path: only Bridge owns its lease and commit gate.
                Action::Create(_) => Err(Error::NotReady),
                Action::Join { link, intent } => self.join(link, intent).await,
                Action::End { session_id, intent } => self.end(&session_id, intent).await,
            }
        })
    }
}

/// A retired binding cannot start even a delayed read after invalidation.
struct EpochTransport<T> {
    inner: Arc<T>,
    epoch: watch::Receiver<u64>,
    verify: Arc<dyn Fn() -> bool + Send + Sync>,
    budget: Arc<Semaphore>,
}

impl<T: Transport> Transport for EpochTransport<T> {
    fn generate<'a>(
        &'a self,
        uri: &'a generation::JoinUri,
        context: &'a Context,
        lease: &'a generation::Lease,
        permit: generation::Permit<'a>,
    ) -> generation::Response<'a> {
        Box::pin(async move {
            let check = || {
                lease.check()?;
                if self.context().ok().as_ref() != Some(context) {
                    return Err(generation::Terminal::Invalidated);
                }
                permit()
            };
            check()?;
            let _slot = self
                .budget
                .acquire()
                .await
                .map_err(|_| generation::Terminal::PreconditionRejected)?;
            check()?;
            let result = self.inner.generate(uri, context, lease, &check).await;
            check()?;
            result
        })
    }
    fn observation_context(&self) -> Result<Context, Error> {
        if !(self.verify)() {
            return Err(Error::Stale);
        }
        self.inner.observation_context()
    }
    fn context(&self) -> Result<Context, Error> {
        if !matches!(self.epoch.has_changed(), Ok(false)) || !(self.verify)() {
            return Err(Error::Stale);
        }
        self.inner.context()
    }
    fn send<'a>(
        &'a self,
        dispatch: Dispatch,
        context: &'a Context,
        permit: Check<'a>,
    ) -> Response<'a> {
        Box::pin(async move {
            let create = dispatch.create.clone();
            let is_create = dispatch.operation == Operation::Create;
            let check = || {
                if let Some(lease) = &create {
                    lease.check()?;
                    if is_create && lease.emitted.load(Ordering::SeqCst) {
                        if self.observation_context()? != *context {
                            return Err(Error::Stale);
                        }
                    } else {
                        self.context()?;
                    }
                } else {
                    self.context()?;
                }
                permit()
            };
            check()?;
            let _budget = if dispatch.operation == Operation::Current {
                if let Some(probe) = &dispatch.diagnostic {
                    probe.stage(Stage::Budget);
                }
                Some(
                    Arc::clone(&self.budget)
                        .acquire_owned()
                        .await
                        .map_err(|_| Error::NotReady)?,
                )
            } else {
                None
            };
            check()?;
            if let Some(probe) = &dispatch.diagnostic {
                probe.stage(Stage::Preparation);
            }
            self.inner.send(dispatch, context, &check).await
        })
    }
}

impl<T: resume::Wire> resume::Wire for EpochTransport<T> {
    fn exchange<'a>(
        &'a self,
        exchange: &'a resume::Exchange,
        context: &'a Context,
        possible: &'a resume::Boundary,
        permit: resume::Permit<'a>,
    ) -> resume::WireResponse<'a> {
        Box::pin(async move {
            let check = || {
                permit()?;
                if self.context().ok().as_ref() != Some(context) {
                    return Err(resume::Terminal::Invalidated);
                }
                Ok(())
            };
            check()?;
            let result = self
                .inner
                .exchange(exchange, context, possible, &check)
                .await;
            check()?;
            result
        })
    }
}

/// Captured on the UI gesture, before the backend command queue.
/// No Debug/Clone: the underlying intent is single-use.
pub struct ResumeRequest {
    stamp: Stamp,
    kernel: Arc<dyn Host>,
    intent: resume::Intent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeReport {
    Waiting,
    Observing,
    Finished(resume::Outcome),
    Cancelled {
        post_possible: bool,
    },
    /// Fixed receipt only: retired HTTP/gate/membership are not deliverable.
    Retired {
        post_possible: bool,
    },
}

type Factory = Arc<dyn Fn(watch::Receiver<u64>) -> Result<Arc<dyn Host>, Error> + Send + Sync>;

fn factory<T: resume::Wire + 'static>(
    make: impl Fn() -> Result<T, Error> + Send + Sync + 'static,
    verify: Arc<dyn Fn() -> bool + Send + Sync>,
    budget: Arc<Semaphore>,
    write_uncertain: Arc<AtomicBool>,
) -> Factory {
    Arc::new(move |mut epoch| {
        epoch.borrow_and_update();
        let transport = EpochTransport {
            inner: Arc::new(make()?),
            epoch,
            verify: Arc::clone(&verify),
            budget: Arc::clone(&budget),
        };
        transport.context()?;
        let mut kernel = Kernel::new(Arc::new(transport), Profile::ObservedFourFields);
        kernel.write_uncertain = Arc::clone(&write_uncertain);
        Ok(Arc::new(kernel))
    })
}

fn unknown(error: Option<Error>) -> Snapshot {
    Snapshot {
        metadata: Default::default(),
        fresh_until: None,
        membership: Membership::Unknown,
        reconcile_required: true,
        error,
    }
}

struct Shared {
    resume_authorization: resume::Authorization,
    resume_used: bool,
    resume_active: bool,
    resume_running: bool,
    resume_cancel: Option<resume::Cancellation>,
    resume_boundary: Option<watch::Receiver<Option<Instant>>>,
    resume_report: Option<(Stamp, ResumeReport)>,
    paused: Arc<dyn Fn() -> bool + Send + Sync>,
    generation_queued: Option<(Arc<AtomicBool>, Instant)>,
    generation_active: bool,
    generation_allowance: Arc<generation::Allowance>,
    generation_lease: Option<Arc<generation::Lease>>,
    generation_report: Option<generation::Terminal>,
    hard_epoch: watch::Sender<u64>,
    pending_create: Option<Arc<CreateLease>>,
    diagnostic: Option<Probe>,
    write_uncertain: Arc<AtomicBool>,
    stamp: Stamp,
    epoch: watch::Sender<u64>,
    output: watch::Sender<Option<Arc<Event>>>,
    latest: Option<Arc<Event>>,
    kernel: Option<Arc<dyn Host>>,
    context: Option<Context>,
    account: Option<String>,
    connection: Option<String>,
    connection_announced: bool,
    listening: bool,
    pending: Option<AbortHandle>,
    verify: Arc<dyn Fn() -> bool + Send + Sync>,
    waker: Waker,
}

impl Shared {
    fn resume_post_possible(&self) -> bool {
        self.resume_boundary
            .as_ref()
            .is_some_and(|b| b.borrow().is_some())
    }
    fn retire_resume(&mut self) {
        let source = self.resume_report.map(|(stamp, _)| stamp);
        let report = ResumeReport::Retired {
            post_possible: self.resume_post_possible(),
        };
        self.cancel_resume(report);
        // Retirement is sticky even after the dispatch task already finished.
        // Preserve receipt provenance, never retag it as current authority.
        if let Some(source) = source
            && self.resume_report != Some((source, report))
        {
            self.resume_report = Some((source, report));
            self.waker.wake();
        }
    }
    fn cancel_resume(&mut self, report: ResumeReport) {
        if let Some(cancel) = self.resume_cancel.take() {
            cancel.cancel();
            if self
                .resume_boundary
                .as_ref()
                .is_some_and(|b| b.borrow().is_some())
            {
                self.write_uncertain.store(true, Ordering::SeqCst);
            }
            self.resume_report = Some((self.stamp, report));
            self.waker.wake();
        }
        if !self.resume_running {
            self.resume_active = false;
        }
        // A running operation remains exclusive until it returns cooperatively.
    }
    fn resume_ready(&self, expected: Stamp) -> bool {
        self.stamp == expected
            && self.context_valid()
            && self.listening
            && self.connection.as_deref().is_some_and(|s| !s.is_empty())
            && self.pending.is_none()
            && !self.resume_active
            && self.generation_queued.is_none()
            && !self.generation_active
            && !self.write_uncertain.load(Ordering::SeqCst)
            && self
                .kernel
                .as_ref()
                .is_some_and(|k| !k.snapshot().reconcile_required)
    }

    fn publish(&mut self, phase: Phase, snapshot: Snapshot) {
        let event = Arc::new(Event {
            stamp: self.stamp,
            phase,
            snapshot,
        });
        self.output.send_replace(Some(Arc::clone(&event)));
        self.latest = Some(event);
        self.waker.wake();
    }

    fn invalidate(&mut self, cause: Cause) {
        self.retire_resume();
        if let Some((cancelled, _)) = self.generation_queued.take() {
            cancelled.store(true, Ordering::SeqCst);
            self.generation_report = Some(generation::Terminal::Invalidated);
        }
        self.generation_active = false;
        if let Some(lease) = self.generation_lease.take() {
            lease.cancel();
            self.generation_report = Some(generation::Terminal::Invalidated);
        }
        let preserve_observation = cause == Cause::Jam
            && self.pending_create.as_ref().is_some_and(|lease| {
                lease.emitted.load(Ordering::SeqCst)
                    && !lease.verifier_emitted.load(Ordering::SeqCst)
                    && lease.check().is_ok()
            });
        if cause != Cause::Jam {
            self.hard_epoch
                .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        }
        if !preserve_observation && let Some(lease) = self.pending_create.take() {
            lease.cancelled.store(true, Ordering::SeqCst);
        }
        if let Some(probe) = &self.diagnostic {
            probe.retire(cause);
        }
        // Fence first, then cancel. A task already running on another thread
        // sees the epoch in its transport checks, not just its result handler.
        self.epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        if let Some(kernel) = self.kernel.take() {
            kernel.invalidate();
        }
        if !preserve_observation && let Some(task) = self.pending.take() {
            task.abort();
        }
        self.context = None;
        self.stamp.revision = self.stamp.revision.wrapping_add(1);
        self.stamp.operation = 0;
        self.publish(Phase::Invalidated(cause), unknown(Some(Error::Stale)));
    }

    fn context_valid(&self) -> bool {
        (self.verify)()
            && self
                .kernel
                .as_ref()
                .is_none_or(|kernel| kernel.context().ok().as_ref() == self.context.as_ref())
    }

    fn reject(&mut self, error: Error) {
        let (phase, mut snapshot) = self
            .latest
            .as_ref()
            .map(|event| (event.phase, event.snapshot.clone()))
            .unwrap_or((Phase::Idle, unknown(None)));
        snapshot.error = Some(error);
        self.publish(phase, snapshot);
    }
}

#[derive(Clone)]
pub(crate) struct Gate(Arc<Mutex<Shared>>);

impl Gate {
    pub(crate) fn resume_available(&self, expected: Stamp) -> bool {
        let state = self.lock();
        !state.resume_used && state.resume_ready(expected) && (state.paused)()
    }
    pub(crate) fn capture_resume(
        &self,
        expected: Stamp,
        gesture: Instant,
    ) -> Result<ResumeRequest, resume::Terminal> {
        let mut state = self.lock();
        if state.resume_used {
            return Err(resume::Terminal::Exhausted);
        }
        if !state.resume_ready(expected) {
            return Err(resume::Terminal::Invalidated);
        }
        let kernel = state.kernel.clone().ok_or(resume::Terminal::Invalidated)?;
        let (intent, cancel) = kernel.resume_intent(
            &state.resume_authorization,
            Arc::clone(&state.paused),
            gesture,
        )?;
        // One captured gesture for this backend lifetime, also on queued failure.
        state.resume_used = true;
        state.resume_active = true;
        state.resume_boundary = Some(intent.post_boundary());
        state.resume_cancel = Some(cancel);
        state.resume_report = Some((expected, ResumeReport::Waiting));
        state.waker.wake();
        Ok(ResumeRequest {
            stamp: expected,
            kernel,
            intent,
        })
    }
    pub(crate) fn cancel_resume(&self) {
        let mut state = self.lock();
        let report = ResumeReport::Cancelled {
            post_possible: state.resume_post_possible(),
        };
        state.cancel_resume(report);
    }
    pub(crate) fn retire_resume(&self) {
        self.lock().retire_resume();
    }
    /// Surface withdrawal discards presentation, never the consumed allowance.
    pub(crate) fn dismiss_resume(&self) {
        let mut state = self.lock();
        let report = ResumeReport::Cancelled {
            post_possible: state.resume_post_possible(),
        };
        state.cancel_resume(report);
        if state.resume_report.take().is_some() {
            state.waker.wake();
        }
    }
    pub(crate) fn resume_report(&self, expected: Stamp) -> Option<ResumeReport> {
        let mut state = self.lock();
        let (source, report) = state.resume_report?;
        // Never deliver into an old view or another account/engine. A receipt
        // has no membership authority and can survive an ordinary read's op++.
        if state.stamp != expected
            || source.account != expected.account
            || source.engine != expected.engine
            || !(state.verify)()
        {
            return None;
        }
        let post_possible = state.resume_post_possible();
        let retired = ResumeReport::Retired { post_possible };
        if source.revision != expected.revision
            || !state.context_valid()
            || (source.operation != expected.operation
                && matches!(report, ResumeReport::Waiting | ResumeReport::Observing))
        {
            state.retire_resume();
            return Some(retired);
        }
        Some(match report {
            ResumeReport::Finished(_) => report,
            ResumeReport::Cancelled { .. } => ResumeReport::Cancelled { post_possible },
            ResumeReport::Retired { .. } => retired,
            _ if source != expected => retired,
            _ if state.resume_active && state.resume_cancel.is_some() && post_possible => {
                ResumeReport::Observing
            }
            _ => report,
        })
    }

    /// Local capability bit only; never exposes the private URI to presentation.
    pub(crate) fn generation_available(&self, expected: Stamp) -> bool {
        let state = self.lock();
        state.stamp == expected
            && state.context_valid()
            && state.listening
            && state
                .connection
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            && state.pending.is_none()
            && !state.resume_active
            && state.generation_queued.is_none()
            && !state.generation_allowance.exhausted()
            && state
                .kernel
                .as_ref()
                .is_some_and(|kernel| kernel.generation_until().is_ok())
    }
    /// Install cancellation before the backend queue, without capturing secrets.
    pub(crate) fn arm_generation(&self, intent: &generation::Intent) -> bool {
        let mut state = self.lock();
        if state.pending.is_some() || state.resume_active || state.generation_queued.is_some() {
            state.generation_report = Some(generation::Terminal::Busy);
            state.waker.wake();
            return false;
        }
        if let Some(lease) = state.generation_lease.take() {
            lease.cancel();
        }
        state.generation_report = Some(generation::Terminal::Waiting);
        state.generation_queued = Some((Arc::clone(&intent.cancelled), intent.deadline));
        true
    }
    pub(crate) fn generation_report(&self) -> Option<generation::Terminal> {
        let mut state = self.lock();
        if state
            .generation_queued
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
        {
            if let Some((cancelled, _)) = state.generation_queued.take() {
                cancelled.store(true, Ordering::SeqCst);
            }
            if let Some(kernel) = &state.kernel {
                kernel.purge_generation();
            }
            state.generation_report = Some(generation::Terminal::Deadline);
        }
        if !state.context_valid() {
            state.generation_report = state
                .generation_report
                .map(|_| generation::Terminal::Invalidated);
        } else if let Some(reason) = state
            .generation_lease
            .as_ref()
            .and_then(|lease| lease.check().err())
        {
            state.generation_report = state.generation_report.map(|_| reason);
        }
        if state.generation_report != Some(generation::Terminal::Accepted(generation::Family::Long))
            && !(state.generation_active
                && state.generation_report == Some(generation::Terminal::Waiting))
            && let Some(lease) = &state.generation_lease
        {
            lease.purge_share();
        }
        state.generation_report
    }
    /// Only a finished, still-live operation can supply private presentation data.
    pub(crate) fn generation_invitation(&self, expected: Stamp) -> Option<metadata::Invitation> {
        self.with_generation_invitation(expected, |invitation| invitation)
    }
    pub(crate) fn with_generation_invitation<T>(
        &self,
        expected: Stamp,
        apply: impl FnOnce(metadata::Invitation) -> T,
    ) -> Option<T> {
        self.generation_report();
        let state = self.lock();
        if state.generation_active {
            return None;
        }
        if state.stamp != expected
            || !state.context_valid()
            || state.generation_report
                != Some(generation::Terminal::Accepted(generation::Family::Long))
        {
            if let Some(lease) = &state.generation_lease {
                lease.purge_share();
            }
            return None;
        }
        Some(apply(state.generation_lease.as_ref()?.invitation()?))
    }
    pub(crate) fn cancel_generation(&self) {
        let mut state = self.lock();
        if let Some((cancelled, _)) = state.generation_queued.take() {
            cancelled.store(true, Ordering::SeqCst);
            if let Some(kernel) = &state.kernel {
                kernel.purge_generation();
            }
            state.generation_report = Some(generation::Terminal::Cancelled);
            state.waker.wake();
        }
        if let Some(lease) = state.generation_lease.take() {
            lease.cancel();
            if state.generation_active
                && let Some(task) = state.pending.take()
            {
                task.abort();
            }
            state.generation_active = false;
            if let Some(kernel) = &state.kernel {
                kernel.purge_generation();
            }
            state.generation_report = Some(generation::Terminal::Cancelled);
            state.waker.wake();
        }
    }
    /// Independent of membership delivery: a coalesced invalidation must still
    /// expose the fixed retirement reason, never stale authority or identity.
    pub(crate) fn diagnostic(&self) -> Option<diagnostic::Diagnostic> {
        let state = self.lock();
        let probe = state.diagnostic.as_ref()?;
        if !state.context_valid() {
            probe.retire(Cause::Context);
        }
        Some(probe.snapshot())
    }

    pub(crate) fn refuse_comparison(&self, error: Error) {
        let mut state = self.lock();
        if let Some(probe) = &state.diagnostic
            && probe.is_comparison()
        {
            probe.finish(Err(error));
            state.reject(error);
        }
    }
    pub(crate) fn cancel_comparison(&self) {
        let mut state = self.lock();
        if state.diagnostic.as_ref().is_some_and(Probe::is_comparison)
            && let Some(probe) = state.diagnostic.take()
        {
            probe.retire(Cause::Context);
        }
    }
    pub(crate) fn arm_diagnostic(&self, action: &Action) {
        let mut state = self.lock();
        if let Some(probe) = state.diagnostic.take() {
            probe.retire(Cause::Context);
        }
        state.diagnostic = match action {
            Action::DiagnoseCurrent(probe) => {
                probe.wake_with(state.waker.clone());
                Some(probe.clone())
            }
            _ => None,
        };
        state.waker.wake();
    }
    /// Local safety only. This survives retired bindings and conveys no remote
    /// identity, membership, freshness or permission to the presentation.
    pub(crate) fn write_uncertain(&self) -> bool {
        self.lock().write_uncertain.load(Ordering::SeqCst)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn stamp(&self) -> Stamp {
        self.lock().stamp
    }
    pub(crate) fn accepts(&self, stamp: Stamp) -> bool {
        let state = self.lock();
        state.stamp == stamp && state.context_valid()
    }
    /// A local private-data emission must also check a context getter that may
    /// have changed before its notification, and hold the stamp through apply.
    pub(crate) fn with_context<T>(&self, stamp: Stamp, apply: impl FnOnce() -> T) -> Option<T> {
        let state = self.lock();
        (state.stamp == stamp && state.context_valid()).then(apply)
    }
    pub(crate) fn accepts_event(&self, event: &Event) -> bool {
        let state = self.lock();
        // An invalid context must still clear the consumer's old projection.
        // Only the latest stamp can deliver that invalidation; requests and
        // completions continue to require a valid context through accepts().
        state.stamp == event.stamp
            && (matches!(event.phase, Phase::Invalidated(_)) || state.context_valid())
    }
    fn signal(&self, engine: u64, notice: Notice) {
        let mut state = self.lock();
        if state.stamp.engine != engine || !state.listening {
            return;
        }
        let cause = match notice {
            Notice::Jam => Cause::Jam,
            Notice::Connection(connection) => {
                state.connection = Some(connection.unwrap_or_default());
                state.connection_announced = true;
                Cause::Connection
            }
            Notice::Closed => {
                state.listening = false;
                Cause::ListenerClosed
            }
        };
        state.invalidate(cause);
    }
}

enum Notice {
    Jam,
    Connection(Option<String>),
    Closed,
}

/// Internal result envelope; no public constructor or private Debug output.
pub struct Completed {
    stamp: Stamp,
    kind: Kind,
    snapshot: Snapshot,
}

/// Private completion. Contains fixed report enums, no generated URL.
pub struct GenerationCompleted {
    stamp: Stamp,
    lease: Arc<generation::Lease>,
    report: generation::Terminal,
}

pub(crate) struct Bridge {
    gate: Gate,
    factory: Option<Factory>,
    watcher: Option<AbortHandle>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::channel(Waker::default()).0
    }
}

impl Bridge {
    pub(crate) fn channel(waker: Waker) -> (Self, watch::Receiver<Option<Arc<Event>>>) {
        let (output, receiver) = watch::channel(None);
        let state = Shared {
            resume_authorization: resume::Authorization::explicitly_authorized(),
            resume_used: false,
            resume_active: false,
            resume_running: false,
            resume_cancel: None,
            resume_boundary: None,
            resume_report: None,
            paused: Arc::new(|| false),
            generation_queued: None,
            generation_active: false,
            generation_allowance: Arc::new(generation::Allowance::default()),
            generation_lease: None,
            generation_report: None,
            hard_epoch: watch::channel(0).0,
            pending_create: None,
            diagnostic: None,
            write_uncertain: Arc::new(AtomicBool::new(false)),
            stamp: Stamp::default(),
            epoch: watch::channel(0).0,
            output,
            latest: None,
            kernel: None,
            context: None,
            account: None,
            connection: None,
            connection_announced: false,
            listening: false,
            pending: None,
            verify: Arc::new(|| true),
            waker,
        };
        (
            Self {
                gate: Gate(Arc::new(Mutex::new(state))),
                factory: None,
                watcher: None,
            },
            receiver,
        )
    }

    pub(crate) fn gate(&self) -> Gate {
        self.gate.clone()
    }

    pub(crate) fn retire(&mut self, account: u64, cause: Cause) {
        self.factory = None;
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        let mut state = self.gate.lock();
        state.invalidate(cause);
        state.diagnostic = None;
        state.stamp.account = account;
        state.stamp.engine = state.stamp.engine.wrapping_add(1);
        state.account = None;
        state.connection = None;
        state.connection_announced = false;
        state.listening = false;
        state.verify = Arc::new(|| true);
        state.publish(Phase::Invalidated(cause), unknown(None));
    }

    fn bind(
        &mut self,
        account: u64,
        name: String,
        factory: Factory,
        verify: Arc<dyn Fn() -> bool + Send + Sync>,
        connection: Option<String>,
        listening: bool,
    ) {
        self.retire(account, Cause::Engine);
        self.factory = Some(factory);
        let mut state = self.gate.lock();
        state.account = Some(name);
        state.verify = verify;
        state.connection = connection;
        state.connection_announced = false;
        state.listening = listening;
        state.publish(
            Phase::Idle,
            unknown(if listening {
                None
            } else {
                Some(Error::NotReady)
            }),
        );
    }

    /// Consume the engine's already reserved local subscriptions. No HTTP,
    /// late Dealer registration, subscription repair or new Session connection.
    pub(crate) fn attach(
        &mut self,
        prepared: PreparedListeners,
        account_epoch: watch::Receiver<u64>,
        api: Arc<crate::api::ApiGateway>,
        budget: Arc<Semaphore>,
        paused: Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        let PreparedListeners {
            session,
            jam,
            connections,
        } = prepared;
        let Some(account) = api.account() else {
            self.retire(*account_epoch.borrow(), Cause::Account);
            return;
        };
        let expected = account.as_str().to_owned();
        let verified = expected.clone();
        let verify: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            api.account()
                .is_some_and(|account| account.as_str() == verified)
        });
        let epoch = *account_epoch.borrow();
        let connection = session.connection_id();
        let make = factory(
            move || SessionTransport::new(session.clone(), account_epoch.clone()),
            Arc::clone(&verify),
            budget,
            Arc::clone(&self.gate.lock().write_uncertain),
        );
        let listening = jam.is_ok() && connections.is_ok();
        self.bind(
            epoch,
            expected,
            make,
            verify,
            (!connection.is_empty()).then_some(connection),
            listening,
        );
        self.gate.lock().paused = paused;
        if let (Ok(mut jam), Ok(mut connections)) = (jam, connections) {
            let gate = self.gate();
            let engine = gate.stamp().engine;
            let task = tokio::spawn(async move {
                loop {
                    let notice = tokio::select! {
                        message = std::future::poll_fn(|cx| jam.as_mut().poll_next(cx)) => match message {
                            Some(Ok(())) => Notice::Jam,
                            _ => Notice::Closed,
                        },
                        message = std::future::poll_fn(|cx| connections.as_mut().poll_next(cx)) => match message {
                            Some(Ok(connection)) => Notice::Connection(connection),
                            _ => Notice::Closed,
                        },
                    };
                    let closed = matches!(notice, Notice::Closed);
                    gate.signal(engine, notice);
                    if closed {
                        break;
                    }
                }
            });
            self.watcher = Some(task.abort_handle());
        }
    }

    pub(crate) fn resume(
        &mut self,
        request: ResumeRequest,
        account: Option<&str>,
        epoch: u64,
        budget: Arc<Semaphore>,
    ) {
        let mut state = self.gate.lock();
        if state.stamp != request.stamp
            || state.account.as_deref() != account
            || state.stamp.account != epoch
            || !state.context_valid()
            || state.resume_cancel.is_none()
            || !state.resume_active
        {
            state.retire_resume();
            state.resume_active = false;
            return;
        }
        state.resume_running = true;
        let gate = self.gate.clone();
        let mut cancelled = request.intent.cancellation();
        let mut boundary = request.intent.post_boundary();
        let stamp = request.stamp;
        tokio::spawn(async move {
            let work = request.kernel.resume(request.intent, budget);
            tokio::pin!(work);
            let observation = async {
                loop {
                    let started = *boundary.borrow_and_update();
                    if let Some(started) = started {
                        gate.lock().waker.wake();
                        tokio::time::sleep_until(started + std::time::Duration::from_secs(10))
                            .await;
                        break;
                    }
                    if boundary.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::pin!(observation);
            let outcome = tokio::select! {
                outcome = &mut work => {
                    if outcome.post_possible {
                        // Observe only the remaining original ten seconds; no new I/O.
                        tokio::select! {
                            _ = &mut observation => (),
                            _ = cancelled.changed() => (),
                        }
                    }
                    outcome
                },
                _ = &mut observation => {
                    if let Some(cancel) = &gate.lock().resume_cancel { cancel.cancel(); }
                    work.await
                },
            };
            let mut state = gate.lock();
            if outcome.post_possible && outcome.terminal != resume::Terminal::Accepted {
                state.write_uncertain.store(true, Ordering::SeqCst);
            }
            state.resume_active = false;
            state.resume_running = false;
            if state.stamp == stamp && state.context_valid() && state.resume_cancel.is_some() {
                state.resume_report = Some((stamp, ResumeReport::Finished(outcome)));
                state.resume_cancel = None;
            }
            state.waker.wake();
        });
    }

    pub(crate) fn request(
        &mut self,
        request: Request,
        account: Option<&str>,
        epoch: u64,
        commands: tokio::sync::mpsc::UnboundedSender<Command>,
    ) {
        let mut state = self.gate.lock();
        let diagnostic = match &request.action {
            Action::DiagnoseCurrent(probe) => Some(probe.clone()),
            _ => None,
        };
        // Backend::send installs the visible handle before queuing. An older
        // queued command must never replace a newer diagnostic intention.
        if let Some(probe) = &diagnostic {
            probe.stage(Stage::Preflight);
        }
        let reject = |state: &mut Shared, error| {
            if let Some(probe) = &diagnostic {
                probe.finish(Err(error));
            }
            state.reject(error);
        };
        if state.account.as_deref() != account
            || state.stamp.account != epoch
            || !state.context_valid()
        {
            if !state.connection_announced {
                state.connection = None;
            }
            state.invalidate(Cause::Context);
            reject(&mut state, Error::Stale);
            return;
        }
        if state.pending.is_some() || state.resume_active {
            reject(&mut state, Error::Busy);
            return;
        }
        if let Some(lease) = state.generation_lease.take() {
            lease.cancel();
            state.generation_report = Some(generation::Terminal::Invalidated);
        }
        if request.expected != state.stamp {
            reject(&mut state, Error::Stale);
            return;
        }
        if !state.listening {
            reject(&mut state, Error::NotReady);
            return;
        }
        if state.kernel.is_none() {
            if request.action.kind() != Kind::Current {
                reject(&mut state, Error::ReconcileRequired);
                return;
            }
            let result = self
                .factory
                .as_ref()
                .ok_or(Error::NotReady)
                .and_then(|make| make(state.epoch.subscribe()));
            match result {
                Ok(kernel) => {
                    let context = kernel.context();
                    match context {
                        Ok(context)
                            if Some(context.account.as_str()) == state.account.as_deref()
                                && state
                                    .connection
                                    .as_ref()
                                    .is_none_or(|connection| *connection == context.connection) =>
                        {
                            state.context = Some(context);
                            state.connection = state
                                .context
                                .as_ref()
                                .map(|context| context.connection.clone());
                            state.connection_announced = false;
                            state.kernel = Some(kernel);
                        }
                        Ok(context)
                            if Some(context.account.as_str()) == state.account.as_deref()
                                && !state.connection_announced =>
                        {
                            // A getter changed before its notification was observed.
                            // Retire the old stamp without issuing a request; the
                            // caller must deliberately read with the new stamp.
                            state.connection = Some(context.connection);
                            state.invalidate(Cause::Connection);
                            reject(&mut state, Error::Stale);
                            return;
                        }
                        _ => {
                            reject(&mut state, Error::NotReady);
                            return;
                        }
                    }
                }
                Err(error) => {
                    reject(&mut state, error);
                    return;
                }
            }
        }
        let kernel = Arc::clone(state.kernel.as_ref().expect("bound kernel"));
        if let Some(probe) = &diagnostic
            && probe.is_comparison()
        {
            let result = if state.write_uncertain.load(Ordering::SeqCst) {
                Err(Error::WriteUncertain)
            } else {
                probe.bind_host(&kernel.snapshot())
            };
            if let Err(error) = result {
                reject(&mut state, error);
                return;
            }
        }
        state.stamp.operation = state.stamp.operation.wrapping_add(1);
        let stamp = state.stamp;
        let kind = request.action.kind();
        if kind == Kind::Create {
            let lease = Arc::new(CreateLease::new(state.hard_epoch.subscribe()));
            state.pending_create = Some(Arc::clone(&lease));
            state.publish(Phase::Pending(kind), unknown(None));
            let factory = Arc::clone(self.factory.as_ref().expect("bound factory"));
            let gate = self.gate.clone();
            let task = tokio::spawn(async move {
                let result = tokio::time::timeout_at(lease.deadline, async {
                    let ack = kernel.capture_create(Arc::clone(&lease)).await?;
                    lease.check()?;
                    let (verifier, stamp) = {
                        let mut state = gate.lock();
                        if !state
                            .pending_create
                            .as_ref()
                            .is_some_and(|active| Arc::ptr_eq(active, &lease))
                            || !(state.verify)()
                        {
                            return Err(Error::Stale);
                        }
                        lease.check()?;
                        let verifier = factory(state.epoch.subscribe())?;
                        let context = verifier.context()?;
                        if context != ack.context {
                            return Err(Error::Stale);
                        }
                        state.kernel = Some(Arc::clone(&verifier));
                        state.context = Some(context);
                        state.stamp.operation = state.stamp.operation.wrapping_add(1);
                        let stamp = state.stamp;
                        state.publish(Phase::Pending(Kind::Create), unknown(None));
                        (verifier, stamp)
                    };
                    let snapshot = verifier.verify_create(ack, Arc::clone(&lease)).await?;
                    Ok::<_, Error>((stamp, snapshot))
                })
                .await
                .unwrap_or(Err(Error::Timeout));
                let completed = {
                    let state = gate.lock();
                    if !state
                        .pending_create
                        .as_ref()
                        .is_some_and(|active| Arc::ptr_eq(active, &lease))
                    {
                        return;
                    }
                    match result {
                        Ok((stamp, snapshot)) => Completed {
                            stamp,
                            kind: Kind::Create,
                            snapshot,
                        },
                        Err(error) => {
                            // A queued failure has no observation left to preserve.
                            lease.cancelled.store(true, Ordering::SeqCst);
                            Completed {
                                stamp: state.stamp,
                                kind: Kind::Create,
                                snapshot: unknown(Some(error)),
                            }
                        }
                    }
                };
                let _ = commands.send(Command::JamFinished(Box::new(completed)));
            });
            state.pending = Some(task.abort_handle());
            return;
        }
        state.publish(Phase::Pending(kind), unknown(None));
        let gate = self.gate.clone();
        let task = tokio::spawn(async move {
            let result = async {
                if !gate.accepts(stamp) {
                    return Err(Error::Stale);
                }
                kernel.run(request.action).await.map(|_| ())
            }
            .await;
            let snapshot = if let Err(error) = result {
                kernel.invalidate();
                let mut snapshot = kernel.snapshot();
                snapshot.error = Some(error);
                snapshot
            } else {
                kernel.snapshot()
            };
            if let Some(probe) = &diagnostic {
                probe.finish(
                    result
                        .as_ref()
                        .map(|_| &snapshot.membership)
                        .map_err(|error| *error),
                );
            }
            let _ = commands.send(Command::JamFinished(Box::new(Completed {
                stamp,
                kind,
                snapshot,
            })));
        });
        state.pending = Some(task.abort_handle());
    }

    pub(crate) fn finish(&mut self, completed: Completed, account: Option<&str>, epoch: u64) {
        let mut state = self.gate.lock();
        if state.stamp != completed.stamp {
            return;
        }
        if state.account.as_deref() != account
            || state.stamp.account != epoch
            || !state.context_valid()
        {
            if !state.connection_announced {
                state.connection = None;
            }
            state.invalidate(Cause::Context);
            return;
        }
        state.pending = None;
        if completed.kind == Kind::Create
            && let Some(lease) = state.pending_create.take()
            && completed.snapshot.error.is_none()
        {
            if lease.check().is_err()
                || !completed
                    .snapshot
                    .fresh_until
                    .is_some_and(|until| Instant::now() < until)
                || !lease.verified.load(Ordering::SeqCst)
                || !lease.verifier_emitted.load(Ordering::SeqCst)
            {
                state.publish(Phase::Complete(Kind::Create), unknown(Some(Error::Stale)));
                return;
            }
            state
                .kernel
                .as_ref()
                .expect("verified kernel")
                .commit_create();
        }
        if completed.kind == Kind::Current
            && let Some(probe) = &state.diagnostic
        {
            probe.commit_comparison(&completed.snapshot);
        }
        state.publish(Phase::Complete(completed.kind), completed.snapshot);
    }

    pub(crate) fn generate(
        &mut self,
        intent: generation::Intent,
        account: Option<&str>,
        epoch: u64,
        commands: tokio::sync::mpsc::UnboundedSender<Command>,
    ) {
        use generation::Terminal as T;
        let mut state = self.gate.lock();
        if state
            .generation_queued
            .as_ref()
            .is_some_and(|(queued, _)| !Arc::ptr_eq(queued, &intent.cancelled))
        {
            return; // A withdrawn old command cannot steal a newer intention.
        }
        state.generation_queued = None;
        if intent.cancelled.load(Ordering::SeqCst) {
            if state.generation_report != Some(T::Deadline) {
                state.generation_report = Some(T::Cancelled);
            }
            state.waker.wake();
            return;
        }
        // Busy does not replace the active lease, abort handle or allowance.
        if state.pending.is_some() || state.resume_active {
            state.generation_report = Some(T::Busy);
            state.waker.wake();
            return;
        }
        let refusal = if state.generation_allowance.exhausted() {
            Some(T::Exhausted)
        } else if Instant::now() >= intent.deadline {
            Some(T::Deadline)
        } else if state.stamp != intent.expected
            || state.account.as_deref() != account
            || state.stamp.account != epoch
            || !state.context_valid()
            || !state.listening
            || state.connection.as_deref().is_none_or(str::is_empty)
        {
            Some(T::PreconditionRejected)
        } else {
            None
        };
        if let Some(report) = refusal {
            if let Some(kernel) = &state.kernel {
                kernel.purge_generation();
            }
            state.generation_report = Some(report);
            state.waker.wake();
            return;
        }
        let Some(kernel) = state.kernel.clone() else {
            state.generation_report = Some(T::PreconditionRejected);
            state.waker.wake();
            return;
        };
        let until = match kernel.generation_until() {
            Ok(until) => until,
            Err(report) => {
                state.generation_report = Some(report);
                state.waker.wake();
                return;
            }
        };
        let lease = Arc::new(generation::Lease::new(
            state.epoch.subscribe(),
            Arc::clone(&state.generation_allowance),
            until,
            intent.deadline,
        ));
        state.generation_lease = Some(Arc::clone(&lease));
        state.generation_report = Some(T::Waiting);
        state.generation_active = true;
        state.stamp.operation = state.stamp.operation.wrapping_add(1);
        let stamp = state.stamp;
        let phase = state
            .latest
            .as_ref()
            .map_or(Phase::Idle, |event| event.phase);
        state.publish(phase, kernel.snapshot());
        let gate = self.gate.clone();
        let task = tokio::spawn(async move {
            let report = if gate.accepts(stamp) {
                kernel.generate(Arc::clone(&lease)).await
            } else {
                T::Invalidated
            };
            let _ = commands.send(Command::JamGenerationFinished(Box::new(
                GenerationCompleted {
                    stamp,
                    lease,
                    report,
                },
            )));
        });
        state.pending = Some(task.abort_handle());
    }

    pub(crate) fn finish_generation(
        &mut self,
        completed: GenerationCompleted,
        account: Option<&str>,
        epoch: u64,
    ) {
        use generation::Terminal as T;
        let mut state = self.gate.lock();
        if state.stamp != completed.stamp
            || !state.generation_active
            || !state
                .generation_lease
                .as_ref()
                .is_some_and(|lease| Arc::ptr_eq(lease, &completed.lease))
        {
            return;
        }
        state.pending = None;
        state.generation_active = false;
        state.generation_report = Some(
            if state.account.as_deref() != account
                || state.stamp.account != epoch
                || !state.context_valid()
            {
                T::Invalidated
            } else {
                completed.lease.check().err().unwrap_or(completed.report)
            },
        );
        if state.generation_report != Some(T::Accepted(generation::Family::Long)) {
            completed.lease.purge_share();
        }
        state.waker.wake();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.retire(self.gate.stamp().account, Cause::Engine);
    }
}

#[cfg(test)]
mod tests;
