---
title: Experimental Jam Resume adapter
description: Backend-only preparation, not a product playback control.
---

This adapter is an isolated candidate for one authorized experiment. It is not
connected to Play, Toggle, startup, refresh, or Dealer notifications. HTTP
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
Dealer listener or a second Session. These are integration requirements for a
future experimental caller; no UI/CLI caller is installed by this preparation.

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
