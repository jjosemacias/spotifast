---
title: Experimental Jam Resume adapter
description: Explicit single-use caller for the approved backend adapter.
---

This adapter is an isolated candidate for one authorized experiment. The queue diagnostic offers an explicit Resume action only in the experimental
startup mode. It is not connected to Play, Toggle, startup, refresh, or Dealer notifications. HTTP
acceptance does not establish audio recovery or synchronization. It does not
change the existing playback controls.

`Kernel<SessionTransport>::experimental_resume_intent` captures an explicit
experimental gesture before its first poll. A caller must keep the same
`resume::Authorization` across context changes, Kernel replacements, and any
surface reopen. A new authorization requires a separately authorized attempt.
The first admitted attempt consumes the grant, even if preparation fails.
Ordinary playback and ordinary Jam reads do not consult this grant.

Call `experimental_resume` only on the backend runtime, with the backend's
existing `background_session` semaphore. The local-paused predicate supplied at
capture must read live engine state on every check. The SessionTransport must
come from that registered local engine and its lifecycle watch channel. Session
device ID is the local descriptor; no active/remote device ID is substituted.
The hosting backend must call `Kernel::invalidate` for Jam changes, including
replacement by an identical projection. This API does not install a second
Dealer listener or a second Session. These requirements are implemented by the existing lifecycle Bridge and the
registered engine. The Bridge retains one Authorization for its entire lifetime,
including retire, bind and surface reopen. Capture reserves exclusion before the
backend queue; pending, reconciliation and uncertainty refuse capture. The
engine predicate locks its current LocalState and requires connected, paused,
a track, no loading and no shutdown. It is never an optimistic UI snapshot.

The adapter invokes Current once, through the existing Session request builder
and bounded response parser. It captures the exact root field `redirectCommands`
before projection consumes the JSON. A present Jam with a fresh literal `true`
is required. Missing, false, invalid, snake-only or conflicting fields cannot
authorize the POST. Gate authority stays in the attempt's memory and is never
published into the product snapshot.

The only possible mutation is one POST to the same Session's HTTPS spclient
origin, using escaped session/local-device path segments and a new JSON body
with command endpoint `resume`. It uses that Session's Login5 bearer and client
token, the current connection ID, JSON headers and an intent-scoped 32-hex
command ID. The Session HTTP client's domain limiter remains active. No
independent H1 client, limiter exception, redirect handling, application retry,
local fallback, seek, Join or Transfer is added. Hyper may recover a request
which has not started writing. This is not exactly-once delivery on the server.

The gesture starts a 20-second dispatch deadline. Gate freshness lasts less
than 15 seconds from receipt of Current, without renewal. Cancellation, paused
state, Kernel revision, account/device/connection/generation and deadlines are
checked around controlled polls and before request construction. Cancellation
wakes the waiting operation and drops the owned auth future; it does not detach
auth into another task. Dropping the cancellation handle also closes the attempt.
Dependency code is cooperative: synchronous Login5/client-token challenge work
can finish an internal auxiliary request before returning control. At return,
late/cancelled auth cannot invoke Current or social POST, and late replies cannot
restore authority. Dispatch deadlines are not hard interruption guarantees for
that synchronous work. An already possible emission remains uncertain on cancel
or transport failure.

Privacy is deliberately strict for this experiment. Before any Session
preparation, and at every controlled poll, it requires `log::max_level() == Off`
and no tracing subscriber (global or current scope). Keep that configuration
fixed from before starting the Session until after observation. The adapter
only refuses unsafe configurations; it does not change logging globally.
This also covers dependency warn/error paths which can print Dealer messages,
HTTP URLs or receiver errors, rather than just the token Trace paths. No raw
errors, tokens, identifiers or response bodies are logged or persisted by the
adapter. The hosting application's other logging/panic sinks and prestate still
require review before a real run; this check is not retroactive log deletion.

Additional I/O can include access-point resolution, DNS/TLS/proxy setup, Login5,
client-token requests/challenges, HTTP429 waits/retries in auth, and independent
Dealer maintenance/reconnection. Current1 plus POST1 is not a total request
count. A real experiment needs separate authorization for prestate and this I/O,
review of the exact caller/build/protocol, and a finite observation window without
another Current or automatic retry. The backend API and its offline fake tests
do not validate credential compatibility, recipient behavior, or catch-up.

## Experimental caller (offline candidate, review required)

A future authorized run must start a fresh process with `--experimental-jam-resume`
and feature `jam-host`. This is an explicit startup switch, not a saved setting,
IPC command, or authorization to prepare accounts/audio. An already running
ordinary instance cannot be upgraded into this mode. The switch conflicts with
verbose, a link and a remote control command. Before creating any Session it
installs a silent logger, facade max level Off, no log/panic files, and a panic
hook which discards its payload. It refuses an existing logger or tracing
subscriber. It skips fastframe-log entirely, so RUST_LOG is not parsed and
cannot override the filter. No other logger/subscriber is installed by this
path. The adapter continues to verify log/tracing at its controlled polls.
The hook does not forward to the ordinary panic sink. This is not retroactive
sanitization for a previously started Session.

In the queue's existing diagnostic section, `Resume social once (experimental)`
emits an action with the gesture time; App captures the actual Kernel Intent
synchronously before sending its backend command. The original 20 seconds
includes the command queue and semaphore wait. Bridge uses the same Kernel,
Session lifecycle, uncertainty flag and background_session budget as ordinary
Jam work. A captured attempt cannot be replaced, automatically retried or
renewed after failure, finish, retire or rebind. Play/Toggle are unchanged.
The caller blocks ordinary Jam work for its queued, dispatch and observation
intervals. An ordinary action cannot take the reservation between capture and
backend execution. Closing/hiding/minimizing/leaving the queue, switching
account, engine, connection or Jam (including identical replacement) cancels.
A running future owns auth and returns cooperatively; it is not aborted while
another auth future keeps polling. A queued cancellation releases its local
reservation while permanently retaining the consumed caller allowance.

The adapter now exposes a timestamped conservative Boundary: Current and POST
mark it immediately before request_fut, including a synchronous limiter refusal.
The timestamp is operational only, not proof that bytes reached Spotify. The
caller watches the POST boundary and ends observation at boundary +10 seconds,
including time awaiting HTTP headers. It adds no Current, polling, collector,
audio capture or local fallback. At the end it cancels any still pending request
cooperatively. It never starts a fresh ten seconds when a late response arrives.
Context retirement ends observation early; late results cannot cross the stamp.
A post-possible failure or cancellation retains Bridge write uncertainty.
A successful HTTP result is displayed independently of the manual audio and
position comparison. Fixed Debug output contains only ResumeReport/Outcome
classifications, HTTP number, gate class and possible-emission booleans.
Cancellation can occur after possible POST: STOP, without another attempt.

Review/build/prestate/human I/O permissions remain separate. The planning
handoff records exact offline commands, tests and the future human protocol.
This caller does not implement creation, joining or prestate audio preparation.

## Retired receipts and manual reads

A Jam/connection retirement within the same account and engine retains only a
fixed `Retired { post_possible }` diagnostic. It cannot retain an HTTP conclusion,
gate, membership or dispatch permission. Retirement is monotonic, including a
getter change before its notice. A completed receipt can survive an ordinary
manual Current's operation increment; that is a historical attempt result, not
current Jam authority. Account/engine withdrawal or closing the diagnostic
surface removes presentation. Stopping an active attempt retains a sanitized
cancelled receipt; Stop is shown only while Waiting/Observing. None of these
paths replenish the captured attempt or Authorization.

The ten-second POST-boundary timer, owned auth future and cooperative fences
are unchanged. Playing while headers remain pending can invalidate the attempt,
but does not create an infinite observation. No automatic Current is added.
A separately requested Current can finish after observation even when a prior
POST remains uncertain. Its valid Presence is shown as `Last check: Jam present`
(or no Jam reported), while membership authority, metadata and mutations stay
blocked by the existing reconciliation/uncertainty policy. A read does not prove
an earlier mutation finished and never clears that uncertainty. This correction
does not establish catch-up or synchronization and does not change endpoint,
authentication, invitation parsing or the social command body.
