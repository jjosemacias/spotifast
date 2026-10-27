//! Explicit, unconnected experimental Resume. No product Toggle/Play routing.
//!
//! One attempt per explicit Authorization, including pre-send failures. A future
//! backend caller must supply its existing background_session semaphore and a
//! live local-paused predicate. Construction/capture performs no network I/O.
use super::*;
use std::task::Poll;
use tokio::sync::Semaphore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Enabled,
    Disabled,
    Missing,
    SnakeOnly,
    Conflict,
    InvalidType,
    Unknown,
}
impl Gate {
    pub(super) fn capture(root: &serde_json::Value) -> Self {
        if root.get("redirect_commands").is_some() {
            return if root.get("redirectCommands").is_some() {
                Self::Conflict
            } else {
                Self::SnakeOnly
            };
        }
        match root.get("redirectCommands") {
            Some(serde_json::Value::Bool(true)) => Self::Enabled,
            Some(serde_json::Value::Bool(false)) => Self::Disabled,
            None => Self::Missing,
            _ => Self::InvalidType,
        }
    }
}

/// Fixed classifications only. HTTP acceptance says nothing about catch-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Accepted,
    Http(u16),
    Busy,
    Exhausted,
    Cancelled,
    Deadline,
    Invalidated,
    GateRejected,
    PrivacyBlocked,
    Preparation,
    Transport,
    InvalidResponse,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub terminal: Terminal,
    /// Sanitized classification only, never a reusable dispatch capability.
    /// Cleared on cancellation, invalidation, deadline and failed dispatch.
    pub gate: Gate,
    /// Conservative application boundaries, not proof of bytes on the wire.
    pub current_possible: bool,
    pub post_possible: bool,
}

/// An explicit experimental gesture, captured synchronously before first poll.
/// No Debug/Serialize: account, device, connection and revision remain in RAM.
pub struct Intent {
    owner: Arc<()>,
    allowance: Arc<AtomicBool>,
    context: Context,
    revision: u64,
    deadline: Instant,
    cancelled: watch::Receiver<bool>,
    paused: Arc<dyn Fn() -> bool + Send + Sync>,
    command_id: String,
}
/// Keep this same grant across Kernel/context replacement and surface reopen.
/// Creating another grant requires a separate authorization, never a retry.
pub struct Authorization(Arc<AtomicBool>);
impl Authorization {
    pub fn explicitly_authorized() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}
pub struct Cancellation(watch::Sender<bool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

/// Real request choices, separate from the product's mutation enum/builder.
/// No Debug/Serialize or raw diagnostic output.
pub enum Exchange {
    Current,
    Social { session: String, command_id: String },
}
pub type Permit<'a> = &'a (dyn Fn() -> Result<(), Terminal> + Send + Sync);
pub type WireResponse<'a> = Pin<Box<dyn Future<Output = Result<Reply, Terminal>> + Send + 'a>>;

/// Narrow injection seam on the existing Jam transport for offline tests.
pub trait Wire: Transport {
    fn exchange<'a>(
        &'a self,
        exchange: &'a Exchange,
        context: &'a Context,
        possible: &'a AtomicBool,
        permit: Permit<'a>,
    ) -> WireResponse<'a>;
}

fn map_error(error: Error) -> Terminal {
    match error {
        Error::Http(status) => Terminal::Http(status),
        Error::Stale | Error::SnapshotExpired => Terminal::Invalidated,
        Error::InvalidResponse => Terminal::InvalidResponse,
        _ => Terminal::Transport,
    }
}

// Check both sides of every poll we own. A dependency's synchronous code is
// cooperative, not preemptible; never detach auth onto another running task.
async fn fenced<F: Future>(future: F, permit: Permit<'_>) -> Result<F::Output, Terminal> {
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        if let Err(error) = permit() {
            return Poll::Ready(Err(error));
        }
        let result = future.as_mut().poll(cx);
        if let Err(error) = permit() {
            return Poll::Ready(Err(error));
        }
        result.map(Ok)
    })
    .await
}

impl<T: Wire> Kernel<T> {
    /// Separate backend-only gesture. The predicate must read actual local
    /// playback state on every check, not a captured optimistic UI boolean.
    pub fn experimental_resume_intent(
        &self,
        authorization: &Authorization,
        paused: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(Intent, Cancellation), Terminal> {
        let context = self.sync_context().map_err(map_error)?;
        if !paused() {
            return Err(Terminal::Invalidated);
        }
        let (cancel, cancelled) = watch::channel(false);
        Ok((
            Intent {
                owner: Arc::clone(&self.resume_identity),
                allowance: Arc::clone(&authorization.0),
                context,
                revision: self.state().revision,
                deadline: Instant::now() + DEADLINE,
                cancelled,
                paused,
                command_id: format!("{:032x}", rand::random::<u128>()),
            },
            Cancellation(cancel),
        ))
    }

    /// Never called by normal Play/Toggle, startup, Dealer, refresh or timers.
    /// `budget` must be this backend's existing background_session semaphore.
    pub async fn experimental_resume(&self, intent: Intent, budget: Arc<Semaphore>) -> Outcome {
        let current = AtomicBool::new(false);
        let post = AtomicBool::new(false);
        let mut gate = Gate::Unknown;
        let terminal = self
            .resume_attempt(&intent, budget, &current, &post, &mut gate)
            .await;
        let terminal = terminal.unwrap_or_else(|error| error);
        if !matches!(terminal, Terminal::Accepted | Terminal::GateRejected) {
            gate = Gate::Unknown;
        }
        Outcome {
            terminal,
            gate,
            current_possible: current.load(Ordering::SeqCst),
            post_possible: post.load(Ordering::SeqCst),
        }
    }

    async fn resume_attempt(
        &self,
        intent: &Intent,
        budget: Arc<Semaphore>,
        current: &AtomicBool,
        post: &AtomicBool,
        observed_gate: &mut Gate,
    ) -> Result<Terminal, Terminal> {
        let check = || {
            if *intent.cancelled.borrow() {
                return Err(Terminal::Cancelled);
            }
            if Instant::now() >= intent.deadline {
                return Err(Terminal::Deadline);
            }
            if !Arc::ptr_eq(&intent.owner, &self.resume_identity) || !(intent.paused)() {
                return Err(Terminal::Invalidated);
            }
            if self.write_uncertain.load(Ordering::SeqCst) {
                return Err(Terminal::Invalidated);
            }
            self.check(&intent.context, intent.revision)
                .map_err(map_error)
        };
        check()?;
        let _gate = self.gate.try_lock().map_err(|_| Terminal::Busy)?;
        intent
            .allowance
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| Terminal::Exhausted)?;
        let operation = async {
            let _slot = fenced(budget.acquire(), &check)
                .await?
                .map_err(|_| Terminal::Preparation)?;
            check()?;
            let reply = fenced(
                self.transport
                    .exchange(&Exchange::Current, &intent.context, current, &check),
                &check,
            )
            .await??;
            check()?;
            let received = Instant::now();
            let read = read_reply(reply, &self.profile).map_err(map_error)?;
            *observed_gate = read.resume_gate;
            let Membership::Present(projection) = read.membership else {
                return Err(Terminal::GateRejected);
            };
            if read.resume_gate != Gate::Enabled {
                return Err(Terminal::GateRejected);
            }
            let social_check = || {
                check()?;
                if Instant::now() >= received + FRESH_FOR {
                    return Err(Terminal::Invalidated);
                }
                Ok(())
            };
            social_check()?;
            let exchange = Exchange::Social {
                session: projection.session_id,
                command_id: intent.command_id.clone(),
            };
            let reply = fenced(
                self.transport
                    .exchange(&exchange, &intent.context, post, &social_check),
                &social_check,
            )
            .await??;
            social_check()?;
            // No body, IDs, gate or result is published back into Kernel state.
            Ok(if reply.status == 200 {
                Terminal::Accepted
            } else {
                Terminal::Http(reply.status)
            })
        };
        let mut cancelled = intent.cancelled.clone();
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err(Terminal::Cancelled),
            result = tokio::time::timeout_at(intent.deadline, operation) => {
                check()?;
                result.map_err(|_| Terminal::Deadline)?
            }
        }
    }
}

// Conservatively require the whole dependency logging chain to be silent.
// This experiment does not reconfigure the logger or inspect ENV/private logs.
// Keep logger configuration fixed from before Session startup through observation.
fn logging_safe(max: log::LevelFilter, tracing_active: bool) -> bool {
    max == log::LevelFilter::Off && !tracing_active
}
fn privacy() -> Result<(), Terminal> {
    let tracing_active = tracing::dispatcher::has_been_set()
        || tracing::dispatcher::get_default(|dispatch| {
            !dispatch.is::<tracing::subscriber::NoSubscriber>()
        });
    if logging_safe(log::max_level(), tracing_active) {
        Ok(())
    } else {
        Err(Terminal::PrivacyBlocked)
    }
}

fn request(
    exchange: &Exchange,
    context: &Context,
    base: &str,
    country: &str,
    bearer: &str,
    client_token: &str,
) -> Result<Request<Vec<u8>>, Terminal> {
    let base_url = reqwest::Url::parse(base).map_err(|_| Terminal::Preparation)?;
    if base_url.scheme() != "https"
        || base_url.host_str().is_none()
        || !base_url.username().is_empty()
        || base_url.password().is_some()
    {
        return Err(Terminal::Preparation);
    }
    if matches!(exchange, Exchange::Current) {
        return prepare_request(
            &Operation::Current,
            context,
            base,
            country,
            rand::random(),
            bearer,
            Some(client_token),
        )
        .map(sensitive_headers)
        .map_err(|_| Terminal::Preparation);
    }
    let Exchange::Social {
        session,
        command_id,
    } = exchange
    else {
        unreachable!()
    };
    if !safe_id(session)
        || context.device.is_empty()
        || matches!(context.device.as_str(), "." | "..")
        || context.device.len() > 2048
        || command_id.len() != 32
        || !command_id.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(Terminal::Preparation);
    }
    let mut url = reqwest::Url::parse(base).map_err(|_| Terminal::Preparation)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Terminal::Preparation);
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|_| Terminal::Preparation)?
        .clear()
        .extend([
            "social-connect",
            "v2",
            "sessions",
            session,
            "commands",
            "from",
            &context.device,
        ]);
    let body = serde_json::to_vec(
        &serde_json::json!({"command": {"endpoint": "resume", "logging_params": {
            "page_instance_ids": [], "interaction_ids": [], "command_id": command_id
        }}}),
    )
    .map_err(|_| Terminal::Preparation)?;
    Request::builder()
        .method(Method::POST)
        .uri(url.as_str())
        .header(http::header::AUTHORIZATION, bearer)
        .header("client-token", client_token)
        .header("spotify-connection-id", &context.connection)
        .header(http::header::ACCEPT, "application/json")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .map(sensitive_headers)
        .map_err(|_| Terminal::Preparation)
}

fn sensitive_headers(mut request: Request<Vec<u8>>) -> Request<Vec<u8>> {
    for name in ["authorization", "client-token", "spotify-connection-id"] {
        if let Some(value) = request.headers_mut().get_mut(name) {
            value.set_sensitive(true);
        }
    }
    request
}

impl Wire for SessionTransport {
    fn exchange<'a>(
        &'a self,
        exchange: &'a Exchange,
        context: &'a Context,
        possible: &'a AtomicBool,
        permit: Permit<'a>,
    ) -> WireResponse<'a> {
        Box::pin(async move {
            let check = || {
                permit()?;
                self.check(context).map_err(map_error)?;
                privacy()?;
                Ok(())
            };
            check()?;
            let spclient = self.session.spclient();
            let base = fenced(spclient.base_url(), &check)
                .await?
                .map_err(|_| Terminal::Preparation)?;
            check()?;
            let login = self.session.login5();
            let token = fenced(login.auth_token(), &check)
                .await?
                .map_err(|_| Terminal::Preparation)?;
            check()?;
            if token.token_type != "Bearer" || token.access_token.is_empty() {
                return Err(Terminal::Preparation);
            }
            let bearer = format!("Bearer {}", token.access_token);
            drop(token);
            let client_token = fenced(spclient.client_token(), &check)
                .await?
                .map_err(|_| Terminal::Preparation)?;
            check()?;
            if client_token.is_empty() {
                return Err(Terminal::Preparation);
            }
            let request = request(
                exchange,
                context,
                &base,
                &self.session.country(),
                &bearer,
                &client_token,
            )?;
            check()?;
            // Burn before the conservative request_fut boundary, even if the
            // private Session domain limiter refuses synchronously. No retry.
            possible.store(true, Ordering::SeqCst);
            let future = self
                .session
                .http_client()
                .request_fut(request.map(Into::into))
                .map_err(|_| Terminal::Preparation)?;
            let response = fenced(future, &check)
                .await?
                .map_err(|_| Terminal::Transport)?;
            check()?;
            if matches!(exchange, Exchange::Social { .. }) {
                return Ok(Reply {
                    status: response.status().as_u16(),
                    content_type: None,
                    body: Vec::new(),
                });
            }
            let body_check = || check().map_err(|_| Error::Stale);
            let reply = fenced(receive_reply(response, &body_check, None), &check).await?;
            check()?;
            reply.map_err(map_error)
        })
    }
}

#[cfg(test)]
mod tests;
