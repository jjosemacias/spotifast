//! Backend binding, passive Dealer invalidation and deliberate Jam requests.
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Semaphore, watch};
use tokio::task::AbortHandle;

use super::*;
use crate::backend::{Command, Waker};

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
            Self::Current => Kind::Current,
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
trait Host: Send + Sync {
    fn context(&self) -> Result<Context, Error>;
    fn invalidate(&self);
    fn snapshot(&self) -> Snapshot;
    fn run(&self, action: Action) -> Work<'_>;
}

impl<T: Transport> Host for Kernel<T> {
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
                Action::Create(intent) => self.create(intent).await,
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
            let check = || {
                self.context()?;
                permit()
            };
            check()?;
            let _budget = if dispatch.operation == Operation::Current {
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
            self.inner.send(dispatch, context, &check).await
        })
    }
}

type Factory = Box<dyn Fn(watch::Receiver<u64>) -> Result<Arc<dyn Host>, Error> + Send + Sync>;

fn factory<T: Transport + 'static>(
    make: impl Fn() -> Result<T, Error> + Send + Sync + 'static,
    verify: Arc<dyn Fn() -> bool + Send + Sync>,
    budget: Arc<Semaphore>,
    write_uncertain: Arc<AtomicBool>,
) -> Factory {
    Box::new(move |mut epoch| {
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
        // Fence first, then cancel. A task already running on another thread
        // sees the epoch in its transport checks, not just its result handler.
        self.epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        if let Some(kernel) = self.kernel.take() {
            kernel.invalidate();
        }
        if let Some(task) = self.pending.take() {
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

    /// Local subscriptions only, on the engine's existing Dealer. No HTTP,
    /// owner inference, subscription repair or connection of another Session.
    pub(crate) fn attach(
        &mut self,
        session: Session,
        account_epoch: watch::Receiver<u64>,
        api: Arc<crate::api::ApiGateway>,
        budget: Arc<Semaphore>,
    ) {
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
        let jam = session
            .dealer()
            .listen_for("social-connect/v2/session_update", |message| {
                if let librespot_core::dealer::protocol::PayloadValue::Json(json) = &message.payload
                {
                    // Strict separate decoder; no delta may establish membership or
                    // ordering. Invalid payloads must also retire the old binding.
                    let _ = metadata::dealer(json.as_bytes());
                }
                Ok(())
            });
        let connections = session
            .dealer()
            .listen_for("hm://pusher/v1/connections/", |message| {
                Ok(message.headers.get("Spotify-Connection-Id").cloned())
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

    pub(crate) fn request(
        &mut self,
        request: Request,
        account: Option<&str>,
        epoch: u64,
        commands: tokio::sync::mpsc::UnboundedSender<Command>,
    ) {
        let mut state = self.gate.lock();
        if state.account.as_deref() != account
            || state.stamp.account != epoch
            || !state.context_valid()
        {
            if !state.connection_announced {
                state.connection = None;
            }
            state.invalidate(Cause::Context);
            state.reject(Error::Stale);
            return;
        }
        if state.pending.is_some() {
            state.reject(Error::Busy);
            return;
        }
        if request.expected != state.stamp {
            state.reject(Error::Stale);
            return;
        }
        if !state.listening {
            state.reject(Error::NotReady);
            return;
        }
        if state.kernel.is_none() {
            if !matches!(request.action, Action::Current) {
                state.reject(Error::ReconcileRequired);
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
                            state.reject(Error::Stale);
                            return;
                        }
                        _ => {
                            state.reject(Error::NotReady);
                            return;
                        }
                    }
                }
                Err(error) => {
                    state.reject(error);
                    return;
                }
            }
        }
        let kernel = Arc::clone(state.kernel.as_ref().expect("bound kernel"));
        state.stamp.operation = state.stamp.operation.wrapping_add(1);
        let stamp = state.stamp;
        let kind = request.action.kind();
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
            if let Err(error) = result {
                kernel.invalidate();
                let mut snapshot = kernel.snapshot();
                snapshot.error = Some(error);
                let _ = commands.send(Command::JamFinished(Box::new(Completed {
                    stamp,
                    kind,
                    snapshot,
                })));
            } else {
                let _ = commands.send(Command::JamFinished(Box::new(Completed {
                    stamp,
                    kind,
                    snapshot: kernel.snapshot(),
                })));
            }
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
        state.publish(Phase::Complete(completed.kind), completed.snapshot);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.retire(self.gate.stamp().account, Cause::Engine);
    }
}

#[cfg(test)]
mod tests;
