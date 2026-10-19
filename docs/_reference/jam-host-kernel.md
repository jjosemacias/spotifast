---
title: Candidate Jam Host Kernel
description: Development-only lifecycle contracts and offline coverage.
permalink: /jam-host-kernel/
nav_exclude: true
---

The `jam-host` Cargo feature is off by default. It exposes `backend::jam`
without connecting it to UI actions, settings, startup, playback, or sign-in.
Compiling or constructing a kernel sends nothing. This is local development
toward Jam support, not a usable or remotely validated Jam feature.

## Candidate protocol

The proposed [protocol description at 09785618](https://github.com/Tu2525/librespot/blob/09785618ef782b746821315b7548617f96671148/docs/jam.md#requests)
is an experimental third-party source, not Spotify documentation. This code
was implemented independently; the related PR935 is not independent evidence
that the protocol works. Exact successful status codes and global end effects
remain unverified.

- Current reads `GET /social-connect/v2/sessions/current`; only its 404 means
  this account is absent.
- Create sends `GET /social-connect/v2/sessions/current_or_new` with
  `activate=true`, `local_device_id`, and `type=REMOTE`. Although GET, this
  is treated as a mutation. It requires an explicit intent and a confirmed
  absent current snapshot less than 15 seconds old. A readable owner response
  and a subsequent current response with the same ID and owner flag are required.
- End sends `DELETE /social-connect/v3/sessions/{id}` with `local_device_id`.
  It requires a fresh owner snapshot, a matching safe ID, and the caller's
  explicit confirmation of its intended effect on everyone. A successful
  response followed by current404 establishes only this account's absence.
  It does not prove global termination or playback recovery for other users.

No join, member leave, invite, members, permission, kick, queue, or audio
implementation is included. The earlier member-leave HTTP405 is unresolved.

## Transport and lifetime

Use `SessionTransport` only with an existing engine after Connect registration.
Its lifecycle watch receiver must advance on logout and engine replacement;
closing the channel also invalidates it. It binds to the initial account,
device, Dealer connection, and generation. It cannot follow a replacement
engine. Create a new controller only after deliberate lifecycle reconciliation.

Requests use the existing Session's SpClient base URL, Login5 authorization,
optional client token, and `product=0`, `country`, `salt` query context. This
matches the canonical helper context in the existing dependency, not a proven
fix for HTTP405. Requests have an empty body, zero Content-Length, JSON Accept,
and `spotify-connection-id`. They use `HttpClient::request_fut`, retaining
proxy, TLS, User-Agent and the local limiter. They do not call helpers with
HTTP429 retries or SpClient reconnection loops, or modify shared retry policy.
At most one application dispatch is made per mutating intent. No claim is
made about unmeasured internal Hyper behavior. Resolving the endpoint and
obtaining auth/client tokens can themselves need network traffic or challenges.
Do not enable credential-bearing dependency trace logs.

The kernel serializes operations and checks the lifecycle and operation
revision after asynchronous preparation and before dispatch/commit. Logout,
replacement, explicit invalidation, cancellation, transport errors, timeouts,
unreadable responses, and unsuccessful verification leave Unknown with a
reconciliation requirement. Rejected writes do not trigger verification or
retry. Only an explicit successful current read permits another write.
Current and the entire mutation/verification sequence each have a 20-second
deadline; a timed-out mutation may already have taken effect remotely.

Choose `Profile::ObservedFourFields` explicitly. Its JSON projection requires
the session ID and all three role/listening/control booleans, accepting snake
or camel case without defaults or alias collisions. It rejects all duplicate
keys, even in ignored metadata; input is limited to 64 KiB, depth32 and
4096 nodes including keys. Only HTTP200 permits absent Content-Type; otherwise
JSON MIME is required. Protobuf, wrappers and explicit missing/invalid MIME
are not coerced into this profile. No global librespot parser is changed.
Identifiers, responses, tokens and contexts have no diagnostic Debug output.

## Offline checks and next integration

`cargo test --locked --no-default-features --features jam-host --lib backend::jam`
uses synthetic transport replies and the real request builder without
authenticating, opening sockets, or creating a playback engine. It covers
request shape, role and freshness gates, one mutation, verification, parser
limits, cancellation, timeouts, concurrent calls and lifecycle fences.

Before enabling runtime actions: review the kernel, integrate lifecycle
invalidation and read budgets with the backend, validate contracts with an
explicitly authorized coordinated host/member test, and approve the UI design.
The backend must call `Kernel::invalidate` on a Jam transition before accepting
another session's events or commands. This unit attaches no Dealer listener;
its operation revision fences that explicit invalidation, and End also requires
the named ID to match the current owner snapshot.
App availability documentation remains unchanged until that evidence exists.
