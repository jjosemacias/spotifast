//! Opt-in, candidate Jam host lifecycle with deliberate backend commands.
//!
//! Reuses a registered engine's Session; never opens a Session or controls audio.
//! Candidate routes: Tu2525/librespot docs/jam.md at 09785618, Requests.
//! The implementation is independent of PR935. Remote semantics remain unverified.

mod comparison;
pub mod diagnostic;
pub mod generation;
pub mod lifecycle;
mod link;
pub mod metadata;
mod projection;
pub use link::Link;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use diagnostic::{Probe, Schema, Stage};
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use librespot_core::session::Session;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::time::Instant;

const MAX_BODY: usize = 65_536;
const FRESH_FOR: Duration = Duration::from_secs(15);
const DEADLINE: Duration = Duration::from_secs(20);

/// Deliberately has no Debug implementation: IDs are not diagnostic output.
#[derive(Clone, PartialEq, Eq)]
pub struct Context {
    pub account: String,
    pub device: String,
    pub connection: String,
    /// The backend's account/engine epoch, advanced on sign-out and replacement.
    pub generation: u64,
}

#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Projection {
    #[serde(alias = "sessionId")]
    pub session_id: String,
    #[serde(alias = "isSessionOwner")]
    pub is_session_owner: bool,
    #[serde(alias = "isListening")]
    pub is_listening: bool,
    #[serde(alias = "isControlling")]
    pub is_controlling: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Membership {
    Unknown,
    Absent,
    Present(Projection),
}

/// Fixed reasons only; never formats a URL, response, token or identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Busy,
    Stale,
    NotReady,
    ReconcileRequired,
    SnapshotExpired,
    NotAbsent,
    NotOwner,
    InvalidId,
    InvalidLink,
    WriteUncertain,
    Http(u16),
    Transport,
    Timeout,
    InvalidResponse,
    VerificationFailed,
}

/// A caller must supply an explicit create intent, not a startup preference.
pub enum CreateIntent {
    Requested,
}

/// Confirmation is for the named Jam and for its effect on everyone.
pub enum EndIntent {
    ForEveryone,
}

/// Requests the observed listen_and_control profile, without claiming its effect.
pub enum JoinIntent {
    ListenAndControlRequested,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Operation {
    Current,
    Create,
    Join(Link),
    End(String),
}

/// Freshness must still hold after token preparation, immediately before send.
pub struct Dispatch {
    create: Option<Arc<lifecycle::CreateLease>>,
    pub operation: Operation,
    pub fresh_until: Option<Instant>,
    uncertainty: Option<Arc<AtomicBool>>,
    diagnostic: Option<Probe>,
}

impl Dispatch {
    /// A synchronous local refusal cannot have emitted HTTP. Only crossing the
    /// first-poll boundary after the final checks creates remote uncertainty.
    async fn poll_prepared<F, T>(
        &self,
        prepared: Result<F, Error>,
        permit: Check<'_>,
    ) -> Result<T, Error>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let future = prepared?;
        permit()?;
        self.check_freshness()?;
        self.mark_emitted();
        if let Some(probe) = &self.diagnostic {
            probe.stage(Stage::Transport);
        }
        future.await
    }

    /// Transport must call at the single HTTP boundary, after all preparation
    /// checks and before yielding to the request future. Cancellation thereafter
    /// cannot prove that the service did not receive a mutation.
    pub fn mark_emitted(&self) {
        if let Some(fence) = &self.uncertainty {
            fence.store(true, Ordering::SeqCst);
        }
        if let Some(lease) = &self.create {
            if self.operation == Operation::Current {
                lease.verifier_emitted.store(true, Ordering::SeqCst);
            } else if self.operation == Operation::Create {
                lease.emitted.store(true, Ordering::SeqCst);
            }
        }
    }
    pub fn check_freshness(&self) -> Result<(), Error> {
        if self
            .fresh_until
            .is_some_and(|until| Instant::now() >= until)
        {
            Err(Error::SnapshotExpired)
        } else {
            Ok(())
        }
    }
}

/// Raw HTTP boundary for injected offline transports. No Debug on private data.
pub struct Reply {
    pub status: u16,
    pub content_type: Option<Vec<u8>>,
    pub body: Vec<u8>,
}

pub type Response<'a> = Pin<Box<dyn Future<Output = Result<Reply, Error>> + Send + 'a>>;
pub type Check<'a> = &'a (dyn Fn() -> Result<(), Error> + Send + Sync);

pub trait Transport: Send + Sync {
    /// Separate experimental POST. Implementations must use the same binding,
    /// check every await and consume Lease::dispatch before request_fut once.
    fn generate<'a>(
        &'a self,
        _uri: &'a generation::JoinUri,
        _context: &'a Context,
        _lease: &'a generation::Lease,
        _permit: generation::Permit<'a>,
    ) -> generation::Response<'a> {
        Box::pin(async { Err(generation::Terminal::PreconditionRejected) })
    }
    /// Must reflect logout, engine replacement and Dealer connection changes.
    fn context(&self) -> Result<Context, Error>;
    /// Physical binding for observing an already emitted Create; grants no authority
    /// or permission to dispatch another request through a retired epoch.
    fn observation_context(&self) -> Result<Context, Error> {
        self.context()
    }
    /// Check `permit` after awaits and both it and dispatch freshness just before
    /// the single application dispatch, calling `Dispatch::mark_emitted` at that
    /// boundary. Never retry or reconnect a mutation.
    fn send<'a>(
        &'a self,
        dispatch: Dispatch,
        context: &'a Context,
        permit: Check<'a>,
    ) -> Response<'a>;
}

/// Existing engine transport. Construction performs no I/O.
pub struct SessionTransport {
    session: Session,
    epoch: watch::Receiver<u64>,
    initial: Context,
}

impl SessionTransport {
    /// Call only after the engine has registered its Connect device. `epoch`
    /// must be the backend's lifecycle channel, invalidated on logout/replacement.
    pub fn new(session: Session, mut epoch: watch::Receiver<u64>) -> Result<Self, Error> {
        let initial = Context {
            account: session.username(),
            device: session.device_id().to_owned(),
            connection: session.connection_id(),
            generation: *epoch.borrow_and_update(),
        };
        let transport = Self {
            session,
            epoch,
            initial,
        };
        transport.context()?;
        Ok(transport)
    }

    fn check(&self, expected: &Context) -> Result<(), Error> {
        if self.context()? == *expected {
            Ok(())
        } else {
            Err(Error::Stale)
        }
    }
}

impl Transport for SessionTransport {
    fn generate<'a>(
        &'a self,
        uri: &'a generation::JoinUri,
        context: &'a Context,
        lease: &'a generation::Lease,
        permit: generation::Permit<'a>,
    ) -> generation::Response<'a> {
        self.generate(uri, context, lease, permit)
    }
    fn context(&self) -> Result<Context, Error> {
        if !matches!(self.epoch.has_changed(), Ok(false))
            || self.session.is_invalid()
            || *self.epoch.borrow() != self.initial.generation
            || self.session.username() != self.initial.account
            || self.session.device_id() != self.initial.device
            || self.session.connection_id() != self.initial.connection
        {
            return Err(Error::Stale);
        }
        if self.initial.account.is_empty()
            || self.initial.device.is_empty()
            || self.initial.connection.is_empty()
        {
            return Err(Error::NotReady);
        }
        Ok(self.initial.clone())
    }

    fn send<'a>(
        &'a self,
        dispatch: Dispatch,
        context: &'a Context,
        permit: Check<'a>,
    ) -> Response<'a> {
        Box::pin(async move {
            let check = || {
                self.check(context)?;
                permit()
            };
            check()?;
            if let Some(probe) = &dispatch.diagnostic {
                probe.stage(Stage::Preparation);
            }
            let base = self
                .session
                .spclient()
                .base_url()
                .await
                .map_err(|_| Error::Transport)?;
            check()?;
            let token = self
                .session
                .login5()
                .auth_token()
                .await
                .map_err(|_| Error::Transport)?;
            check()?;
            // Matches the helper's optional client-token policy, but does not
            // call its request/retry loops. Acquisition may itself need I/O.
            let client_token = self.session.spclient().client_token().await.ok();
            check()?;
            let request = prepare_request(
                &dispatch.operation,
                context,
                &base,
                &self.session.country(),
                rand::random(),
                &format!("{} {}", token.token_type, token.access_token),
                client_token.as_deref(),
            )?;
            check()?;
            dispatch.check_freshness()?;
            let prepared = self
                .session
                .http_client()
                .request_fut(request.map(Into::into))
                .map_err(|_| Error::Transport)
                .map(|future| async move { future.await.map_err(|_| Error::Transport) });
            let response = dispatch.poll_prepared(prepared, &check).await?;
            receive_reply(response, &check, dispatch.diagnostic.as_ref()).await
        })
    }
}

async fn receive_reply<B>(
    response: http::Response<B>,
    check: Check<'_>,
    diagnostic: Option<&Probe>,
) -> Result<Reply, Error>
where
    B: BodyExt + Unpin,
    B::Data: AsRef<[u8]>,
{
    receive_reply_result(response, check, diagnostic)
        .await
        .map_err(|(error, _)| error)
}

// Operation-local result only. HTTP accompanies a failed frame, never a fence.
async fn receive_reply_result<B>(
    response: http::Response<B>,
    check: Check<'_>,
    diagnostic: Option<&Probe>,
) -> Result<Reply, (Error, Option<u16>)>
where
    B: BodyExt + Unpin,
    B::Data: AsRef<[u8]>,
{
    let status = response.status().as_u16();
    if let Some(probe) = diagnostic {
        probe.status(status);
        probe.stage(Stage::Body);
    }
    check().map_err(|error| (error, None))?;
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .map(|value| value.as_bytes().to_vec());
    let mut incoming = response.into_body();
    let mut body = Vec::new();
    while let Some(frame) = incoming.frame().await {
        check().map_err(|error| (error, None))?;
        let frame = frame.map_err(|_| (Error::Transport, Some(status)))?;
        if let Ok(data) = frame.into_data() {
            let data = data.as_ref();
            if data.len() > MAX_BODY - body.len() {
                if let Some(probe) = diagnostic {
                    probe.schema(Schema::BodyLimit);
                }
                return Err((Error::InvalidResponse, None));
            }
            body.extend_from_slice(data);
        }
    }
    check().map_err(|error| (error, None))?;
    Ok(Reply {
        status,
        content_type,
        body,
    })
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 2048
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn prepare_request(
    operation: &Operation,
    context: &Context,
    base: &str,
    country: &str,
    salt: u32,
    authorization: &str,
    client_token: Option<&str>,
) -> Result<Request<Vec<u8>>, Error> {
    let (method, path) = match operation {
        Operation::Current => (
            Method::GET,
            "/social-connect/v2/sessions/current".to_owned(),
        ),
        Operation::Create => (
            Method::GET,
            "/social-connect/v2/sessions/current_or_new".to_owned(),
        ),
        Operation::Join(link) => (
            Method::POST,
            format!("/social-connect/v2/sessions/join/{}", link.0),
        ),
        Operation::End(id) if safe_id(id) => {
            (Method::DELETE, format!("/social-connect/v3/sessions/{id}"))
        }
        Operation::End(_) => return Err(Error::InvalidId),
    };
    let mut url = reqwest::Url::parse(base).map_err(|_| Error::NotReady)?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(Error::NotReady);
    }
    url.set_path(&path);
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        if matches!(operation, Operation::Create) {
            query
                .append_pair("activate", "true")
                .append_pair("local_device_id", &context.device)
                .append_pair("type", "REMOTE");
        } else if matches!(operation, Operation::Join(_)) {
            query
                .append_pair("playback_control", "listen_and_control")
                .append_pair("join_type", "deeplinking")
                .append_pair("local_device_id", &context.device);
        } else if matches!(operation, Operation::End(_)) {
            query.append_pair("local_device_id", &context.device);
        }
        // Canonical SpClient helper context, not a proposed remedy for HTTP405.
        query
            .append_pair("product", "0")
            .append_pair("country", country)
            .append_pair("salt", &salt.to_string());
    }
    let mut builder = Request::builder()
        .method(method)
        .uri(url.as_str())
        .header(http::header::AUTHORIZATION, authorization)
        .header("spotify-connection-id", &context.connection)
        .header(http::header::CONTENT_LENGTH, 0)
        .header(http::header::ACCEPT, "application/json");
    if let Some(token) = client_token {
        builder = builder.header("client-token", token);
    }
    builder.body(Vec::new()).map_err(|_| Error::NotReady)
}

/// Only this explicitly selected profile accepts an absent MIME on HTTP200.
/// No protobuf, wrapper, default roles or global librespot parser changes.
pub enum Profile {
    ObservedFourFields,
}

fn success(reply: &Reply) -> Result<(), Error> {
    if (200..300).contains(&reply.status) {
        Ok(())
    } else {
        Err(Error::Http(reply.status))
    }
}

struct Read {
    generation_uri: Option<generation::JoinUri>,
    membership: Membership,
    metadata: metadata::Metadata,
}

// Private correlation witness only. No metadata or authority survives in it.
struct CreateAck {
    context: Context,
    session_id: String,
}

#[cfg(test)]
fn membership(reply: Reply, profile: &Profile) -> Result<Membership, Error> {
    read_reply(reply, profile).map(|read| read.membership)
}

fn read_reply(reply: Reply, profile: &Profile) -> Result<Read, Error> {
    read_reply_observed(reply, profile, None)
}

fn read_reply_observed(
    reply: Reply,
    _profile: &Profile,
    diagnostic: Option<&Probe>,
) -> Result<Read, Error> {
    if let Some(probe) = diagnostic {
        probe.status(reply.status);
    }
    if reply.status == StatusCode::NOT_FOUND.as_u16() {
        return Ok(Read {
            generation_uri: None,
            membership: Membership::Absent,
            metadata: Default::default(),
        });
    }
    success(&reply)?;
    match &reply.content_type {
        None if reply.status == 200 => (),
        Some(raw)
            if std::str::from_utf8(raw)
                .ok()
                .and_then(|s| s.split(';').next())
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json")) => {}
        _ => {
            if let Some(probe) = diagnostic {
                probe.schema(Schema::Mime);
            }
            return Err(Error::InvalidResponse);
        }
    }
    let classify = |schema| {
        move |error| {
            if let Some(probe) = diagnostic {
                probe.schema(schema);
            }
            error
        }
    };
    let root = projection::json(&reply.body).map_err(classify(if reply.body.len() > MAX_BODY {
        Schema::BodyLimit
    } else {
        Schema::JsonStructure
    }))?;
    let metadata =
        metadata::snapshot_observed(&root, diagnostic).map_err(classify(Schema::Metadata))?;
    let generation_uri = generation::JoinUri::capture(&root);
    let projection = projection::from_value(root).map_err(classify(Schema::Projection))?;
    Ok(Read {
        generation_uri,
        membership: Membership::Present(projection),
        metadata,
    })
}

#[derive(Clone)]
pub struct Snapshot {
    pub metadata: metadata::Metadata,
    /// Authority expires at the kernel deadline, including time queued for UI delivery.
    pub fresh_until: Option<Instant>,
    pub membership: Membership,
    /// Includes cancellation and ambiguous writes. Cleared only by a fresh read.
    pub reconcile_required: bool,
    pub error: Option<Error>,
}

struct State {
    generation_uri: Option<generation::JoinUri>,
    metadata: metadata::Metadata,
    context: Option<Context>,
    revision: u64,
    membership: Membership,
    current_at: Option<Instant>,
    reconcile: bool,
    error: Option<Error>,
}

impl State {
    fn invalidate(&mut self) {
        self.generation_uri = None;
        self.revision = self.revision.wrapping_add(1);
        self.membership = Membership::Unknown;
        self.metadata = Default::default();
        self.current_at = None;
        self.reconcile = true;
        self.error = None;
    }
}

/// No background task or automatic reads. Invoke explicitly on the backend runtime.
pub struct Kernel<T: Transport> {
    transport: Arc<T>,
    profile: Profile,
    gate: AsyncMutex<()>,
    state: Mutex<State>,
    // Shared by replacement bindings. A 404 cannot disprove a late mutation.
    write_uncertain: Arc<AtomicBool>,
}

impl<T: Transport> Kernel<T> {
    async fn capture_create(&self, lease: Arc<lifecycle::CreateLease>) -> Result<CreateAck, Error> {
        let _gate = self.gate.try_lock().map_err(|_| Error::Busy)?;
        let context = self.sync_context()?;
        let (revision, fresh_until) = {
            let mut state = self.state();
            if self.write_uncertain.load(Ordering::SeqCst) {
                return Err(Error::WriteUncertain);
            }
            if state.reconcile {
                return Err(Error::ReconcileRequired);
            }
            if !state.current_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
                return Err(Error::SnapshotExpired);
            }
            if state.membership != Membership::Absent {
                return Err(Error::NotAbsent);
            }
            let fresh_until = state.current_at.map(|at| at + FRESH_FOR);
            state.invalidate();
            (state.revision, fresh_until)
        };
        let permit = || {
            lease.check()?;
            if lease.emitted.load(Ordering::SeqCst) {
                if self.transport.observation_context()? == context {
                    Ok(())
                } else {
                    Err(Error::Stale)
                }
            } else {
                self.check(&context, revision)
            }
        };
        let reply = self
            .transport
            .send(
                Dispatch {
                    create: Some(Arc::clone(&lease)),
                    operation: Operation::Create,
                    fresh_until,
                    uncertainty: Some(Arc::clone(&self.write_uncertain)),
                    diagnostic: None,
                },
                &context,
                &permit,
            )
            .await?;
        permit()?;
        if (400..500).contains(&reply.status)
            && reply.status != 408
            && self.check(&context, revision).is_ok()
        {
            self.write_uncertain.store(false, Ordering::SeqCst);
        }
        success(&reply)?;
        match read_reply(reply, &self.profile)?.membership {
            Membership::Present(session) if session.is_session_owner => Ok(CreateAck {
                context,
                session_id: session.session_id,
            }),
            _ => Err(Error::VerificationFailed),
        }
    }

    async fn verify_create(
        &self,
        ack: CreateAck,
        lease: Arc<lifecycle::CreateLease>,
    ) -> Result<Snapshot, Error> {
        let _gate = self.gate.try_lock().map_err(|_| Error::Busy)?;
        lease.check()?;
        let context = self.sync_context()?;
        if context != ack.context {
            return Err(Error::Stale);
        }
        let revision = {
            let mut state = self.state();
            state.invalidate();
            state.revision
        };
        let permit = || {
            lease.check()?;
            self.check(&context, revision)
        };
        let reply = self
            .transport
            .send(
                Dispatch {
                    create: Some(Arc::clone(&lease)),
                    operation: Operation::Current,
                    fresh_until: None,
                    uncertainty: None,
                    diagnostic: None,
                },
                &context,
                &permit,
            )
            .await?;
        permit()?;
        let read = read_reply(reply, &self.profile)?;
        if !matches!(&read.membership, Membership::Present(session) if session.is_session_owner && session.session_id == ack.session_id)
        {
            return Err(Error::VerificationFailed);
        }
        let mut state = self.state();
        if state.revision != revision || state.context.as_ref() != Some(&context) {
            return Err(Error::Stale);
        }
        let at = Instant::now();
        state.membership = read.membership.clone();
        state.generation_uri = read.generation_uri.clone();
        state.metadata = read.metadata.clone();
        state.current_at = Some(at);
        state.reconcile = true;
        state.error = Some(Error::WriteUncertain);
        lease.verified.store(true, Ordering::SeqCst);
        Ok(Snapshot {
            membership: read.membership,
            metadata: read.metadata,
            fresh_until: Some(at + FRESH_FOR),
            reconcile_required: false,
            error: None,
        })
    }

    fn commit_create(&self) {
        self.write_uncertain.store(false, Ordering::SeqCst);
        let mut state = self.state();
        state.reconcile = false;
        state.error = None;
    }
    pub fn new(transport: Arc<T>, profile: Profile) -> Self {
        Self {
            transport,
            profile,
            gate: AsyncMutex::new(()),
            state: Mutex::new(State {
                generation_uri: None,
                metadata: Default::default(),
                context: None,
                revision: 0,
                membership: Membership::Unknown,
                current_at: None,
                reconcile: true,
                error: None,
            }),
            write_uncertain: Arc::new(AtomicBool::new(false)),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Explicit lifecycle invalidation also fences an outstanding operation.
    pub fn invalidate(&self) {
        let mut state = self.state();
        state.invalidate();
        state.context = None;
    }

    fn sync_context(&self) -> Result<Context, Error> {
        let context = self.transport.context();
        let mut state = self.state();
        if context.as_ref().ok() != state.context.as_ref() {
            state.invalidate();
            state.context = context.as_ref().ok().cloned();
        }
        if let Err(error) = context {
            state.error = Some(error);
            return Err(error);
        }
        context
    }

    pub fn snapshot(&self) -> Snapshot {
        let _ = self.sync_context();
        let mut state = self.state();
        if !state.current_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
            state.generation_uri = None;
            state.metadata = Default::default();
        }
        Snapshot {
            metadata: if !state.reconcile
                && state.current_at.is_some_and(|at| at.elapsed() < FRESH_FOR)
            {
                state.metadata.clone()
            } else {
                Default::default()
            },
            fresh_until: state.current_at.map(|at| at + FRESH_FOR),
            membership: state.membership.clone(),
            reconcile_required: state.reconcile,
            error: state.error,
        }
    }

    fn check(&self, context: &Context, revision: u64) -> Result<(), Error> {
        if self.sync_context()? != *context || self.state().revision != revision {
            Err(Error::Stale)
        } else {
            Ok(())
        }
    }

    async fn read_current(
        &self,
        context: &Context,
        revision: u64,
        diagnostic: Option<&Probe>,
    ) -> Result<Read, Error> {
        self.check(context, revision)?;
        let permit = || {
            self.check(context, revision)?;
            if let Some(probe) = diagnostic {
                probe.check_comparison()?;
            }
            Ok(())
        };
        let reply = self
            .transport
            .send(
                Dispatch {
                    create: None,
                    operation: Operation::Current,
                    fresh_until: diagnostic.and_then(Probe::comparison_fresh_until),
                    uncertainty: None,
                    diagnostic: diagnostic.cloned(),
                },
                context,
                &permit,
            )
            .await?;
        self.check(context, revision)?;
        if let Some(probe) = diagnostic {
            probe.stage(Stage::Response);
        }
        read_reply_observed(reply, &self.profile, diagnostic)
    }

    pub async fn current(&self) -> Result<Membership, Error> {
        self.run(Operation::Current).await
    }

    pub async fn join(&self, link: Link, _intent: JoinIntent) -> Result<Membership, Error> {
        self.run(Operation::Join(link)).await
    }

    /// Returns Absent for this account only, never claims global termination.
    pub async fn end(&self, session_id: &str, _intent: EndIntent) -> Result<Membership, Error> {
        if !safe_id(session_id) {
            return Err(Error::InvalidId);
        }
        self.run(Operation::End(session_id.to_owned())).await
    }

    async fn run(&self, operation: Operation) -> Result<Membership, Error> {
        self.run_observed(operation, None).await
    }

    async fn run_observed(
        &self,
        operation: Operation,
        diagnostic: Option<&Probe>,
    ) -> Result<Membership, Error> {
        // Create is owned by Bridge: capture/verify cannot publish or commit here.
        if operation == Operation::Create {
            return Err(Error::NotReady);
        }
        let _gate = self.gate.try_lock().map_err(|_| Error::Busy)?;
        let context = self.sync_context()?;
        let (revision, fresh_until) = {
            let mut state = self.state();
            if operation != Operation::Current {
                if self.write_uncertain.load(Ordering::SeqCst) {
                    return Err(Error::WriteUncertain);
                }
                if state.reconcile {
                    return Err(Error::ReconcileRequired);
                }
                if !state.current_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
                    return Err(Error::SnapshotExpired);
                }
                match (&operation, &state.membership) {
                    (Operation::Join(_), Membership::Absent) => (),
                    (Operation::Join(_), _) => return Err(Error::NotAbsent),
                    (Operation::End(id), Membership::Present(session))
                        if session.is_session_owner && session.session_id == *id => {}
                    _ => return Err(Error::NotOwner),
                }
            }
            // Set before the first await. Dropping the future cannot leave a
            // reusable success snapshot or permit another write.
            let fresh_until = state.current_at.map(|at| at + FRESH_FOR);
            state.invalidate();
            (state.revision, fresh_until)
        };
        let deadline = diagnostic
            .and_then(Probe::comparison_deadline)
            .unwrap_or_else(|| Instant::now() + DEADLINE);
        let result = tokio::time::timeout_at(deadline, async {
            self.check(&context, revision)?;
            if operation == Operation::Current {
                return self.read_current(&context, revision, diagnostic).await;
            }
            let permit = || self.check(&context, revision);
            let reply = self
                .transport
                .send(
                    Dispatch {
                        create: None,
                        operation: operation.clone(),
                        fresh_until,
                        uncertainty: Some(Arc::clone(&self.write_uncertain)),
                        diagnostic: None,
                    },
                    &context,
                    &permit,
                )
                .await?;
            self.check(&context, revision)?;
            if (400..500).contains(&reply.status) && reply.status != 408 {
                // A definite rejection can be retried only after a deliberate read.
                self.write_uncertain.store(false, Ordering::SeqCst);
            }
            success(&reply)?;
            let joined = if matches!(operation, Operation::Join(_)) {
                match read_reply(reply, &self.profile)? {
                    Read {
                        membership: Membership::Present(session),
                        ..
                    } if !session.is_session_owner => Some(session.session_id),
                    _ => return Err(Error::VerificationFailed),
                }
            } else {
                None
            };
            let current = self.read_current(&context, revision, None).await?;
            match (&operation, joined, &current.membership) {
                (Operation::End(_), _, Membership::Absent) => Ok(current),
                (Operation::Join(_), Some(id), Membership::Present(session))
                    if !session.is_session_owner && session.session_id == id =>
                {
                    Ok(current)
                }
                _ => Err(Error::VerificationFailed),
            }
        })
        .await
        .unwrap_or(Err(Error::Timeout));
        self.check(&context, revision)?;
        let mut state = self.state();
        if state.revision != revision || state.context.as_ref() != Some(&context) {
            return Err(Error::Stale);
        }
        match &result {
            Ok(read) => {
                if operation != Operation::Current {
                    self.write_uncertain.store(false, Ordering::SeqCst);
                }
                state.membership = read.membership.clone();
                state.generation_uri = read.generation_uri.clone();
                state.metadata = read.metadata.clone();
                state.current_at = Some(Instant::now());
                state.reconcile = self.write_uncertain.load(Ordering::SeqCst);
                if state.reconcile {
                    state.error = Some(Error::WriteUncertain);
                }
            }
            Err(error) => state.error = Some(*error),
        }
        result.map(|read| read.membership)
    }
}

#[cfg(test)]
mod tests;
