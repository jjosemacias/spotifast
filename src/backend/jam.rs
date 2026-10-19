//! Opt-in, candidate Jam host lifecycle. Nothing in the app starts this kernel.
//!
//! Reuses a registered engine's Session; never opens a Session or controls audio.
//! Candidate routes: Tu2525/librespot docs/jam.md at 09785618, Requests.
//! The implementation is independent of PR935. Remote semantics remain unverified.

mod projection;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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

#[derive(Clone, PartialEq, Eq)]
pub enum Operation {
    Current,
    Create,
    End(String),
}

/// Freshness must still hold after token preparation, immediately before send.
pub struct Dispatch {
    pub operation: Operation,
    pub fresh_until: Option<Instant>,
}

impl Dispatch {
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
    /// Must reflect logout, engine replacement and Dealer connection changes.
    fn context(&self) -> Result<Context, Error>;
    /// Check `permit` after awaits and both it and dispatch freshness just before
    /// the single application dispatch. Never retry or reconnect a mutation.
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
            let response = self
                .session
                .http_client()
                .request_fut(request.map(Into::into))
                .map_err(|_| Error::Transport)?
                .await
                .map_err(|_| Error::Transport)?;
            check()?;
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .map(|value| value.as_bytes().to_vec());
            let mut incoming = response.into_body();
            let mut body = Vec::new();
            while let Some(frame) = incoming.frame().await {
                check()?;
                let frame = frame.map_err(|_| Error::Transport)?;
                if let Ok(data) = frame.into_data() {
                    if data.len() > MAX_BODY - body.len() {
                        return Err(Error::InvalidResponse);
                    }
                    body.extend_from_slice(&data);
                }
            }
            check()?;
            Ok(Reply {
                status,
                content_type,
                body,
            })
        })
    }
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

fn membership(reply: Reply, _profile: &Profile) -> Result<Membership, Error> {
    if reply.status == StatusCode::NOT_FOUND.as_u16() {
        return Ok(Membership::Absent);
    }
    success(&reply)?;
    match &reply.content_type {
        None if reply.status == 200 => (),
        Some(raw)
            if std::str::from_utf8(raw)
                .ok()
                .and_then(|s| s.split(';').next())
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json")) => {}
        _ => return Err(Error::InvalidResponse),
    }
    let projection = projection::read(&reply.body)?;
    Ok(Membership::Present(projection))
}

pub struct Snapshot {
    pub membership: Membership,
    /// Includes cancellation and ambiguous writes. Cleared only by a fresh read.
    pub reconcile_required: bool,
    pub error: Option<Error>,
}

struct State {
    context: Option<Context>,
    revision: u64,
    membership: Membership,
    current_at: Option<Instant>,
    reconcile: bool,
    error: Option<Error>,
}

impl State {
    fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.membership = Membership::Unknown;
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
}

impl<T: Transport> Kernel<T> {
    pub fn new(transport: Arc<T>, profile: Profile) -> Self {
        Self {
            transport,
            profile,
            gate: AsyncMutex::new(()),
            state: Mutex::new(State {
                context: None,
                revision: 0,
                membership: Membership::Unknown,
                current_at: None,
                reconcile: true,
                error: None,
            }),
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
        let state = self.state();
        Snapshot {
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

    async fn read_current(&self, context: &Context, revision: u64) -> Result<Membership, Error> {
        self.check(context, revision)?;
        let permit = || self.check(context, revision);
        let reply = self
            .transport
            .send(
                Dispatch {
                    operation: Operation::Current,
                    fresh_until: None,
                },
                context,
                &permit,
            )
            .await?;
        self.check(context, revision)?;
        membership(reply, &self.profile)
    }

    pub async fn current(&self) -> Result<Membership, Error> {
        self.run(Operation::Current).await
    }

    pub async fn create(&self, _intent: CreateIntent) -> Result<Membership, Error> {
        self.run(Operation::Create).await
    }

    /// Returns Absent for this account only, never claims global termination.
    pub async fn end(&self, session_id: &str, _intent: EndIntent) -> Result<Membership, Error> {
        if !safe_id(session_id) {
            return Err(Error::InvalidId);
        }
        self.run(Operation::End(session_id.to_owned())).await
    }

    async fn run(&self, operation: Operation) -> Result<Membership, Error> {
        let _gate = self.gate.try_lock().map_err(|_| Error::Busy)?;
        let context = self.sync_context()?;
        let (revision, fresh_until) = {
            let mut state = self.state();
            if operation != Operation::Current {
                if state.reconcile {
                    return Err(Error::ReconcileRequired);
                }
                if !state.current_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
                    return Err(Error::SnapshotExpired);
                }
                match (&operation, &state.membership) {
                    (Operation::Create, Membership::Absent) => (),
                    (Operation::Create, _) => return Err(Error::NotAbsent),
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
        let result = tokio::time::timeout(DEADLINE, async {
            self.check(&context, revision)?;
            if operation == Operation::Current {
                return self.read_current(&context, revision).await;
            }
            let permit = || self.check(&context, revision);
            let reply = self
                .transport
                .send(
                    Dispatch {
                        operation: operation.clone(),
                        fresh_until,
                    },
                    &context,
                    &permit,
                )
                .await?;
            self.check(&context, revision)?;
            success(&reply)?;
            let created = if operation == Operation::Create {
                match membership(reply, &self.profile)? {
                    Membership::Present(session) if session.is_session_owner => {
                        Some(session.session_id)
                    }
                    _ => return Err(Error::VerificationFailed),
                }
            } else {
                None
            };
            let current = self.read_current(&context, revision).await?;
            match (&operation, created, &current) {
                (Operation::Create, Some(id), Membership::Present(session))
                    if session.is_session_owner && session.session_id == id =>
                {
                    Ok(current)
                }
                (Operation::End(_), _, Membership::Absent) => Ok(current),
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
            Ok(membership) => {
                state.membership = membership.clone();
                state.current_at = Some(Instant::now());
                state.reconcile = false;
            }
            Err(error) => state.error = Some(*error),
        }
        result
    }
}

#[cfg(test)]
mod tests;
