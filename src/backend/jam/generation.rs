//! Explicit E08B-1 experiment. Historical reduced request, never product sharing.
use super::*;
mod failure;
mod http1;
pub use failure::{FutureCause, FutureFailure, H2Origin, H2Reason, HyperContext, IoKind, TlsKind};
const DESTINATION: &str = "https://spclient.wg.spotify.com/url-dispenser/v1/generate-url";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Long,
    Short,
}
/// Boundary only, not a diagnosis. Only a received response can carry HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportStage {
    PrepareHttp,
    AwaitHeaders(FutureFailure),
    ReadBody(u16),
}
/// Fixed diagnostic only. Never contains a URI, identity, auth or returned URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Waiting,
    Accepted(Family),
    Busy,
    Exhausted,
    PreconditionRejected,
    AuthUnavailable,
    PrivacyBlocked,
    Cancelled,
    Invalidated,
    Deadline,
    Transport(TransportStage),
    Http(u16),
    SchemaRejected,
    OutputNotCovered,
}
pub type Response<'a> = Pin<Box<dyn Future<Output = Result<Reply, Terminal>> + Send + 'a>>;
pub type Permit<'a> = &'a (dyn Fn() -> Result<(), Terminal> + Send + Sync);

/// Construct only for a separately authorized explicit experimental gesture.
/// Only the explicit diagnostic gesture creates it. Deadline starts before queuing.
pub struct Intent {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) expected: lifecycle::Stamp,
    pub(crate) deadline: Instant,
}
impl Intent {
    pub fn explicitly_authorized(expected: lifecycle::Stamp) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            expected,
            deadline: Instant::now() + DEADLINE,
        }
    }
}

/// One allowance per Bridge lifetime; never replaced by bind/retire/new Jam.
#[derive(Default)]
pub(super) struct Allowance(AtomicBool);
pub struct Lease {
    epoch: watch::Receiver<u64>,
    allowance: Arc<Allowance>,
    pub(super) until: Instant,
    pub(super) deadline: Instant,
    cancelled: AtomicBool,
}
impl Lease {
    pub(super) fn new(
        epoch: watch::Receiver<u64>,
        allowance: Arc<Allowance>,
        until: Instant,
        deadline: Instant,
    ) -> Self {
        Self {
            epoch,
            allowance,
            until,
            deadline,
            cancelled: AtomicBool::new(false),
        }
    }
    pub(super) fn check(&self) -> Result<(), Terminal> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Terminal::Cancelled);
        }
        if !matches!(self.epoch.has_changed(), Ok(false)) {
            return Err(Terminal::Invalidated);
        }
        if Instant::now() >= self.deadline {
            return Err(Terminal::Deadline);
        }
        if Instant::now() >= self.until {
            return Err(Terminal::Invalidated);
        }
        Ok(())
    }
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    /// Consume before calling request_fut, even when its preparation refuses.
    pub fn dispatch(&self) -> Result<(), Terminal> {
        self.check()?;
        self.allowance
            .0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| Terminal::Exhausted)
    }
}
impl Allowance {
    pub(super) fn exhausted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct Auth {
    bearer: String,
    client_token: String,
}
struct Purge<'a, T: Transport>(&'a Kernel<T>);
impl<T: Transport> Drop for Purge<'_, T> {
    fn drop(&mut self) {
        self.0.state().generation_uri = None;
    }
}

fn map_error(error: Error) -> Terminal {
    match error {
        Error::Timeout => Terminal::Deadline,
        Error::InvalidResponse => Terminal::SchemaRejected,
        _ => Terminal::Invalidated,
    }
}

// Match the actual default module_path! targets at the pinned librespot source.
// No env reads or assumptions about product logger defaults or its sinks.
fn auth_logging_safe(logger: &dyn log::Log, level: log::LevelFilter) -> bool {
    level < log::LevelFilter::Trace
        || ["librespot_core::login5", "librespot_core::spclient"]
            .into_iter()
            .all(|target| {
                !logger.enabled(
                    &log::Metadata::builder()
                        .target(target)
                        .level(log::Level::Trace)
                        .build(),
                )
            })
}
fn privacy() -> Result<(), Terminal> {
    if auth_logging_safe(log::logger(), log::max_level()) {
        Ok(())
    } else {
        Err(Terminal::PrivacyBlocked)
    }
}

// Check before EVERY poll of dependency auth, including its internal awaits.
// Runtime logger reconfiguration concurrently inside a poll is not atomic with
// this check. A future experiment must keep its logger configuration fixed.
async fn fenced<F: Future>(future: F, check: Permit<'_>) -> Result<F::Output, Terminal> {
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        if let Err(error) = check() {
            return std::task::Poll::Ready(Err(error));
        }
        future.as_mut().poll(cx).map(Ok)
    })
    .await
}

impl SessionTransport {
    pub(super) fn generate<'a>(
        &'a self,
        uri: &'a JoinUri,
        context: &'a Context,
        lease: &'a Lease,
        permit: Permit<'a>,
    ) -> Response<'a> {
        Box::pin(async move {
            let check = || {
                lease.check()?;
                self.check(context).map_err(map_error)?;
                permit()
            };
            let auth_check = || {
                check()?;
                privacy()
            };
            auth_check()?;
            let login = self.session.login5();
            let token = fenced(login.auth_token(), &auth_check).await?;
            auth_check()?;
            let token = token.map_err(|_| Terminal::AuthUnavailable)?;
            if token.token_type != "Bearer" || token.access_token.is_empty() {
                return Err(Terminal::AuthUnavailable);
            }
            let bearer = format!("Bearer {}", token.access_token);
            drop(token); // Never format Token's Debug implementation.
            let spclient = self.session.spclient();
            let client_token = fenced(spclient.client_token(), &auth_check).await?;
            auth_check()?;
            let client_token = client_token.map_err(|_| Terminal::AuthUnavailable)?;
            let request = request(
                uri,
                Auth {
                    bearer,
                    client_token,
                },
            )?;
            check()?;
            lease.dispatch()?;
            // Explicit H1 mode only, one ephemeral client/invocation. No Session
            // HTTP mutation, dummy reservation, fallback or allowance renewal.
            http1::send(request, self.session.config().proxy.as_ref(), &check).await
        })
    }
}

// Shared by the native call and offline fixtures, without storing raw errors.
async fn http_reply<F, B, P, E>(
    prepare: impl FnOnce() -> Result<F, P>,
    check: Permit<'_>,
    classify: impl FnOnce(E) -> FutureFailure,
) -> Result<Reply, Terminal>
where
    F: Future<Output = Result<http::Response<B>, E>>,
    B: BodyExt + Unpin,
    B::Data: AsRef<[u8]>,
{
    let future = prepare().map_err(|_| Terminal::Transport(TransportStage::PrepareHttp))?;
    let response = fenced(future, check).await?;
    check()?;
    let response = response
        .map_err(|error| Terminal::Transport(TransportStage::AwaitHeaders(classify(error))))?;
    let body_check = || check().map_err(|_| Error::Stale);
    let reply = receive_reply_result(response, &body_check, None).await;
    check()?;
    reply.map_err(|(error, status)| match (error, status) {
        (Error::Transport, Some(status)) => Terminal::Transport(TransportStage::ReadBody(status)),
        (error, _) => map_error(error),
    })
}
// Private RAM only. Deliberately no Debug/Serialize or public string accessor.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinUri(String);

impl JoinUri {
    pub(super) fn capture(root: &serde_json::Value) -> Option<Self> {
        // Exact field only. Never consult URL/token or an alias, even if equal.
        if root.get("joinSessionUri").is_some() {
            return None;
        }
        let value = root.get("join_session_uri")?.as_str()?;
        let tail = value
            .strip_prefix("spotify:")
            .or_else(|| value.strip_prefix("hm:"))?;
        (!tail.is_empty()
            && value.len() <= 2048
            && value.trim() == value
            && !value.chars().any(char::is_control))
        .then(|| Self(value.to_owned()))
    }
}

pub(super) struct Input {
    context: Context,
    revision: u64,
    jam: String,
    owner: String,
    pub(super) until: Instant,
    uri: JoinUri,
}

impl<T: Transport> Kernel<T> {
    pub(super) fn generation_until(&self) -> Result<Instant, Terminal> {
        self.generation_input().map(|input| input.until)
    }
    pub(super) async fn generate(&self, lease: Arc<Lease>) -> Terminal {
        let Ok(_gate) = self.gate.try_lock() else {
            return Terminal::Busy;
        };
        let _purge = Purge(self);
        let input = match self.generation_input() {
            Ok(input) => input,
            Err(error) => return error,
        };
        if lease.until != input.until {
            return Terminal::Invalidated;
        }
        let check = || {
            lease.check()?;
            self.generation_check(&input)
        };
        let result = tokio::time::timeout_at(lease.deadline.min(lease.until), async {
            check()?;
            let reply = self
                .transport
                .generate(&input.uri, &input.context, &lease, &check)
                .await;
            check()?;
            let family = output(reply?)?;
            check()?;
            Ok(Terminal::Accepted(family))
        })
        .await
        .unwrap_or_else(|_| {
            Err(if Instant::now() >= lease.deadline {
                Terminal::Deadline
            } else {
                Terminal::Invalidated
            })
        });
        match check() {
            Err(error) => error,
            Ok(()) => result.unwrap_or_else(|error| error),
        }
    }
    pub(super) fn generation_input(&self) -> Result<Input, Terminal> {
        let context = self
            .sync_context()
            .map_err(|_| Terminal::PreconditionRejected)?;
        let mut state = self.state();
        if state.reconcile || state.error.is_some() || self.write_uncertain.load(Ordering::SeqCst) {
            return Err(Terminal::PreconditionRejected);
        }
        let until = state
            .current_at
            .map(|at| at + FRESH_FOR)
            .filter(|until| Instant::now() < *until)
            .ok_or_else(|| {
                state.generation_uri = None;
                Terminal::PreconditionRejected
            })?;
        let Membership::Present(host) = &state.membership else {
            return Err(Terminal::PreconditionRejected);
        };
        if !host.is_session_owner {
            return Err(Terminal::PreconditionRejected);
        }
        let owner = state
            .metadata
            .owner
            .as_ref()
            .filter(|v| !v.trim().is_empty())
            .ok_or(Terminal::PreconditionRejected)?;
        Ok(Input {
            context,
            revision: state.revision,
            jam: host.session_id.clone(),
            owner: owner.clone(),
            until,
            uri: state
                .generation_uri
                .clone()
                .ok_or(Terminal::PreconditionRejected)?,
        })
    }

    fn generation_check(&self, input: &Input) -> Result<(), Terminal> {
        self.check(&input.context, input.revision)
            .map_err(|_| Terminal::Invalidated)?;
        if Instant::now() >= input.until {
            self.state().generation_uri = None;
            return Err(Terminal::Invalidated);
        }
        let state = self.state();
        if state.reconcile
            || state.error.is_some()
            || self.write_uncertain.load(Ordering::SeqCst)
            || state.metadata.owner.as_ref() != Some(&input.owner)
            || state.generation_uri.as_ref() != Some(&input.uri)
            || !matches!(&state.membership, Membership::Present(host)
                if host.is_session_owner && host.session_id == input.jam)
        {
            return Err(Terminal::Invalidated);
        }
        Ok(())
    }
}

fn request(uri: &JoinUri, auth: Auth) -> Result<http::Request<Vec<u8>>, Terminal> {
    if !auth.bearer.starts_with("Bearer ") || auth.bearer.len() == 7 || auth.client_token.is_empty()
    {
        return Err(Terminal::AuthUnavailable);
    }
    let body = serde_json::to_vec(&serde_json::json!({"spotify_uri": &uri.0}))
        .map_err(|_| Terminal::PreconditionRejected)?;
    let mut request = http::Request::builder()
        .method(Method::POST)
        .uri(DESTINATION)
        .header(http::header::HOST, "spclient.wg.spotify.com")
        .header(http::header::AUTHORIZATION, auth.bearer)
        .header("client-token", auth.client_token)
        .header(http::header::ACCEPT, "application/json")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .map_err(|_| Terminal::AuthUnavailable)?;
    request
        .headers_mut()
        .get_mut(http::header::AUTHORIZATION)
        .unwrap()
        .set_sensitive(true);
    request
        .headers_mut()
        .get_mut("client-token")
        .unwrap()
        .set_sensitive(true);
    Ok(request)
}

fn output(reply: Reply) -> Result<Family, Terminal> {
    if !matches!(reply.status, 200 | 201) {
        return Err(Terminal::Http(reply.status));
    }
    let mime = reply
        .content_type
        .as_deref()
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.split(';').next());
    if !mime.is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json")) {
        return Err(Terminal::SchemaRejected);
    }
    let root = projection::json(&reply.body).map_err(|_| Terminal::SchemaRejected)?;
    if root.get("shareableUrl").is_some() {
        return Err(Terminal::SchemaRejected);
    }
    let url = root
        .get("shareable_url")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 2048)
        .ok_or(Terminal::SchemaRejected)?;
    if url.trim() != url || url.chars().any(char::is_control) {
        return Err(Terminal::OutputNotCovered);
    }
    // Literal prefixes exclude userinfo/ports. No resolution or decoding.
    // Long form uses the existing Link grammar; a short path stays opaque.
    if Link::parse(url).is_ok() {
        return Ok(Family::Long);
    }
    if let Some(segment) = url.strip_prefix("https://spotify.link/")
        && !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment
            .chars()
            .any(|c| c.is_whitespace() || "/\\?#".contains(c))
        && reqwest::Url::parse(url).is_ok_and(|parsed| parsed.as_str() == url)
    {
        return Ok(Family::Short);
    }
    Err(Terminal::OutputNotCovered)
}

#[cfg(test)]
mod tests;
