---
title: Candidate Jam Host Kernel
description: Development-only lifecycle contracts and offline coverage.
permalink: /jam-host-kernel/
nav_exclude: true
---

The `jam-host` Cargo feature is off by default. It adds a native experimental
Jam management to the existing queue and a compact Connect summary and uses `backend::jam` with passive
lifecycle binding on engine acceptance. There are no settings, startup Jam
requests, automatic refreshes or added audio commands. Compiling, constructing
a kernel and opening Connect send no Jam request. Only synthetic offline
validation is available; this is an incomplete local development unit.

## Queue, membership strip and Connect

- Choose **Check Jam state** deliberately. It is a read, never creation. A fresh
  confirmed absence and active local playback enable **Start a Jam**. A stale
  snapshot requires another deliberate check, without automatic chaining.
- The four membership fields are the only current projection. They do not
  supply participants, invitations, policy or shared queue permissions. These
  areas explain their availability without working-looking placeholder controls.
- Only a fresh confirmed host can open **End Jam…**. **Cancel** receives initial
  focus, Tab stays within the confirmation, and Cancel restores its current
  trigger. New controls have 36-point minimum targets. **End for everyone** echoes the observed context and named Jam. Any
  account, engine, connection, revision or operation change discards the intent.
  Snapshot deadlines come from the kernel, including UI delivery delay. A
  counter-only acceptance race holds pending progress until the matching event,
  while a binding change still discards it immediately.
- Each accepted gesture sends at most one backend command. Pending state appears
  immediately and blocks duplicate or incompatible requests. No write is retried.
  Backend serialization and the existing session-read budget still apply, also
  to the kernel's post-write check. There is no lazy pre-write read.
- A successful End currently verifies only this account's absence. UI does not
  claim that global termination, link invalidation or audio changes were observed.
- Connect keeps devices, Create and a summary. Manage Jam opens and focuses the
  queue's Jam heading, selecting Queue instead of Recent. Repeated access never
  toggles the queue closed. The existing Queue page stays a page; no responsive
  switch or new minimum window size is introduced. Creation opens management
  only after fresh confirmed membership. Opening management does not read Current,
  dispatch an extra queue refresh, transfer devices or control playback. Existing
  visible-queue updates continue normally.
- The 36-point strip sits below the unchanged 88-point player. Only fresh
  presence uses the accent background, with Host/Participant text; Unknown,
  expiration and pending use neutral text/surface. Fresh absence removes it.
  Check Jam state on the strip opens recovery; the read requires a separate
  deliberate gesture in management. It does not certify audio synchronization.
- The Jam block precedes queue loading/error/empty states in its existing scroll.
  Recent, Save, local manual/context queues and duplicate occurrences retain
  their existing data and controls. Local queue availability does not certify
  a remote shared queue or permission to edit one.
- Closing Connect, the queue, Escape and outside click never leave or end a Jam. Closing
  preserves a submitted operation; context invalidation clears its authority.
  A closed-popup error produces a discrete notice without opening or focusing it.
- Existing devices, discovery and playback setup remain in Connect. Once Jam is
  deliberately inspected, device transfers are stopped while membership is
  present or uncertain because continuity is not established. Default builds
  retain the existing transfer behavior. No Player/Spirc commands or queue ownership changes.
- Participant Leave (including the unresolved 405), sharing, member
  management, permissions and queue/control capabilities remain separate pending
  product requirements. This feature does not satisfy the complete Jam scope.

For synthetic native preview, build with `--no-default-features --features
jam-host,demo` and use `--demo --demo-show jam-owner` (also `jam-unknown`,
`jam-absent`, `jam-participant`, `jam-pending`, `jam-error`, `jam-end`).
Append `,jam-connect` for Connect, or `,jam-page` to select the existing Queue
page explicitly for a 760x520 preview. `,light` selects the light theme. These
fixtures never supply authority to the backend and use the existing offline demo.
Normal development builds without `jam-host` contain no Jam UI. No new credentials,
saved Jam state or storage migration is introduced. New UI phrases use the
existing gettext fallback; complete catalog extraction and translations remain
an integration gate for this local opt-in unit.

## Temporary local Current diagnostic

**Diagnose one Jam check** is a separate deliberate read in this experimental
feature. It issues the same Current request, with the same parser, deadline,
budget and context guards. Opening the view does not arm it. Ordinary Check and
every mutation clear the visible diagnostic slot and do not instrument requests.

The one in-memory record contains only `class`, `http` (numeric or `none`) and
`schema` (`none`, `body_limit`, `mime`, `json_structure`, `metadata`, `projection`).
Classes distinguish queue, preflight, budget, preparation, request-future wait,
body receipt and response validation, followed by the fixed error or result.
They describe the **last accepted observation**, not the first failure, an HTTP
trace or proof of remote receipt. `http=none` means no status captured at that
boundary, not proof that no request was sent. Status is captured before body
reading, so body errors can retain it. Preparation can itself require existing
session traffic; there is no additional Jam request or retry.

The first connection/Jam/listener invalidation replaces the class with its fixed
cause and seals the record against late updates, even if a Complete watch event was
coalesced away. A loss of verified context additionally clears status/schema;
account/engine replacement removes the slot. The next intention replaces or
clears it, and process exit destroys it. No history, timestamps, identities,
stamps, URLs, response/header values, exception strings, logging or persistence
are added. Updates wake the existing UI directly; there is no diagnostic timer
or network polling. The record never supplies authority to any Jam action.

The diagnostic UI is temporary local development tooling. It has offline unit
coverage only; independent review and a separately authorized human observation
are required before using it with an account.

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

No member leave, invite, members, permission, kick, shared queue, or audio
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
retry. A definite rejection requires an explicit successful current read before
a new gesture. Any ambiguous Jam mutation retains a write fence even after Current404,
Dealer invalidation, engine replacement or sign-out/sign-in in this process.
That read cannot disprove a late remote mutation. There is no automatic replay or
UI reset for this fence; resolve and observe remote state explicitly before a
separately authorized further test. Restarting the process loses this in-memory
fence and is not a remote reconciliation protocol.
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

Before enabling by default: independently review the integration and UI, and
validate contracts with an explicitly authorized coordinated host/member test.
Default app availability remains unchanged until that evidence exists. The
experimental opt-in UI above does not satisfy those gates.

## Backend lifecycle binding

`Command::Jam(lifecycle::Request)` is the only deliberate entry point. Echo
`Backend::jam_stamp()` or the stamp of the last observed state; requests carry
Current, Create intent, or the named End intent. The Connect view emits a UI
Action; app.rs applies it after drawing and sends these deliberate requests.
`Backend::poll_jam()` exposes a separate, coalesced watch state. This keeps
memory bounded even without a UI consumer. Phases distinguish pending,
completion and invalidation, with fixed errors. Completion is not a success
claim: inspect the snapshot's error and membership.

Stamps cover account, engine, binding revision and operation. Old intentions,
queued completions and states awaiting consumption are fenced. Busy rejection
does not replace the identity of the operation already running. Logout,
engine retirement/replacement, lost account verification, and context changes
invalidate before retiring work. A transport epoch is advanced before aborting
tasks, so a delayed read cannot begin after retirement. Only Current requests,
including the kernel's successful-write verification, acquire the backend's
existing four-slot background_session semaphore. Writes remain serialized and
are never retried. The kernel deadline also covers waiting for that read slot.

Withdrawing the last verified Web API identity retires the binding before the
grant changes, even if that same account verifies again before pending work
resumes. A second Ready grant for the same account preserves the binding.
Retirement requires a new engine binding and a deliberate Current; restoring
the account alone cannot revive old results or intentions. The latest Unknown
invalidation remains consumable while verification or context is unavailable.

On engine acceptance, two listeners reuse its Dealer before a read is allowed:
social-connect/v2/session_update and hm://pusher/v1/connections/. JSON Jam payloads
have a separate bounded delta decoder, never the HTTP membership profile. No
delta is applied as member/owner truth: ordering is not demonstrated. Every notification conservatively
invalidates to Unknown, cancels pending work and fences already queued results.
This includes notifications caused by our own writes: they can leave the outcome
uncertain, requiring a new deliberate Current, without retrying the mutation.
The connection header is compared with the Session's installed connection ID;
it is not exposed in events. A missing header blocks readiness. Stream end/error
leaves NotReady until a new engine binding; no subscription repair or polling
is introduced. No notification establishes Absent.

The pin exposes connection identity, not a public acknowledgement that Connect
registration has finished. No readiness timer is invented. These local fences
and candidate requests still require a coordinated real registration/host test;
an engine Ready notification alone is not remote Jam acceptance. The existing
client-token dependency trace caveat remains open before account tests.

The UI consumer releases its watch borrow before taking the lifecycle gate.
Publishers acquire those locks in the opposite order, so retaining the watch
borrow across gate validation could deadlock delivery. A controlled offline
regression reproduces that old order and verifies publication can proceed with
the gate occupied. This changes no protocol, requests or retry behavior.


## Experimental long-link Join (J02-06)

After a deliberate fresh Current404 on the same registered Connect/Dealer
context, **Join with an invitation…** opens a native modal from either Connect
or the queue. Opening, closing and editing send nothing and never read the
clipboard; standard paste is a user gesture. Cancel clears the private draft.
Context changes and expiry retire it. Keyboard playback shortcuts are suspended
while the form is open. No link is saved or included in diagnostic output.

Only the exact `https://open.spotify.com/socialsession/{alphanumeric-token}`
form is accepted, optionally with unique `si` and `utm_source` URI-unreserved
tracking values, validated then discarded. Case changes, ports, credentials,
fragments, encoded tokens, unknown/duplicate parameters and short links are
rejected before any Jam request. Input, token and tracking budgets are bounded.
The parser is adapted from Jose's local librespot experiment at 627b5f0,
`core/src/jam.rs`; no dependency source or observer was ported.

The user explicitly requests listening on this computer. The only founded
request profile is `POST /social-connect/v2/sessions/join/{token}` with empty
body and `playback_control=listen_and_control`, `join_type=deeplinking`, and
`local_device_id`. This parameter expresses a request, not achieved mode or
permission. Host control without local audio remains visibly unavailable;
no alternative endpoint or transfer is guessed. Short links/additional formats
remain requirements pending clarification, not advertised support.

Join is distinct from Create and shares its serialization/fresh absence gate,
without requiring the local player to be active. The engine must already be
registered. Present owner/member, Unknown, stale, pending, reconciliation or
an uncertain previous Join allow no POST. Context and freshness are checked
again after token preparation immediately before the single dispatch. No
Leave, End, transfer, new Session, auth flow, retry or polling is introduced.
A cancellation before emission is fenced without claiming a remote write;
the uncertainty latch is marked at the actual HTTP boundary for Join, Create
and End, so an uncertain earlier host mutation also blocks Join. An absent but stale or
reconciliation-blocked projection cannot enable device transfer either.

A strict Join response must explicitly identify a participant and a subsequent
Current must identify that same session and participant role within the same
context. This correlates the response to the submitted intention, not a general
token-to-session mapping or idempotency guarantee. A repeated link while Present
is refused. An ID, YOU_JOINED or HTTP200 alone cannot confirm membership. Passive
Dealer updates continue to invalidate; they do not manufacture identity/roles.
Listening/control booleans are displayed as service declarations only. Effective
mode, local audio/synchronization and host command authority remain unconfirmed,
with no new playback capability enabled. Disagreement or unreadable data leaves
Unknown and no successful Join message. Definite 4xx rejection (except ambiguous
408), transport failure, timeout, cancellation and reconciliation are tested
separately. No failed mutation is automatically verified or replayed.

Native offline demo scenes add `jam-join`, `jam-join-invalid`,
`jam-join-pending`, `jam-join-uncertain`. They are fixtures, not service evidence.
Backend/lifecycle/model and native demo tests cover the guards, late results,
draft retirement, keyboard focus/cancel and single gesture. Real two-account
membership and audio acceptance remain J02-12/13; this unit does not accept
R03/R04 as complete, M1/M2 or the full Jam feature.


## Local refusal and presentation resets

A synchronous `request_fut` refusal happens before a Hyper response future is
available. It does not mark remote uncertainty. After obtaining the future,
context and freshness are checked again, then the write fence is set immediately
before the first poll. A failed or cancelled future after that point keeps the
fence; there is still no automatic retry. A local refusal still retires the
snapshot and requires a deliberate fresh Current and a new gesture.

Connect's Transfer and ActivateReceiver actions also consult the Bridge's local
write fence directly, independently of presentation engagement. Signing out and
back in clears membership, roles, drafts and confirmation as before, but cannot
turn a possibly outstanding write into permission to transfer. A Current404 does
not remove that safety block. The scalar local fence grants no remote identity,
role, freshness or capability. Without uncertain Jam activity, ordinary Connect
and fresh confirmed absence keep their existing behavior.

## Experimental participants and invitation (J02-08)

The opt-in Jam section can show display names, declared host and avatars from
optional fields on the same strict HTTP membership response. This presentation
DTO is separate from the four mandatory membership fields. Optional names and
owner IDs grant no command authority. A supplied member list is explicitly
partial, with no claimed global count. An omitted list is unavailable; an explicit
empty list means only that this response lists nobody. Names without a member
identity cannot create a participant. Unknown fields remain bounded and duplicate
keys, conflicting aliases, duplicate identities and wrong types are rejected.

Current, Create and Join responses use the same strict HTTP parser and MIME
policy. For mutations the final verifying Current supplies the displayed metadata;
metadata from the earlier mutation response is not merged into an omitted field
of a later snapshot. No format, completeness or ordering guarantee is invented.
Dealer's delta envelope validates optional session/member fields separately;
all Dealer notifications still invalidate and require deliberate Current. An
omitted delta list is never interpreted as an empty snapshot. Applying partial
Dealer changes without a demonstrated ordering contract remains pending.

Only a fresh confirmed owner can open the invitation card in the existing queue.
`join_session_url` accepts the exact long HTTPS form already supported for Join,
or the exact `spotify://sessions/join/{alphanumeric-token}` form, converted to
that long URL. Arbitrary HTTPS origins, short links, fragments, encoded tokens,
unknown parameters and token fields used as a fallback are unavailable. This
format evidence is the candidate implementation at Tu2525/spotifast 5fca13ce,
not an official Spotify guarantee. Other forms need evidence before support.

Opening the card generates a local QR using the small optional qrcodegen encoder
(no transitive dependencies). The QR represents exactly the validated URL offered
to Copy link. It is painted directly with a quiet zone, retained only in memory,
and discarded on close, expiry, pending/Unknown or any binding change. Enlarge QR
opens a native modal; Escape closes it and returns focus. No URL is printed in
the card or used as a texture/cache key. Copy link emits a native clipboard
request only after the application revalidates its stamp, role and deadline.
The existing clipboard integration has no delivery acknowledgement, so the
notice states that delivery is unconfirmed, never that copying succeeded.

Avatar input accepts only a 40-hex Spotify image identifier or its exact
`https://i.scdn.co/image/{id}` form, with no query/credentials/arbitrary hosts.
It uses the existing artwork loader and its ordinary bounded image cache;
no member identity, name, invitation or QR is persisted. Unsupported/missing
pictures use the existing neutral user icon, without a new profile lookup.

Native demo scenes `jam-members`, `jam-members-empty`, `jam-invitation`, and
`jam-qr` contain synthetic identities and a synthetic URL only. They demonstrate
presentation and local actions, not a usable Spotify invitation. Acceptance of
R02 and the invitation-to-Join-to-same-Jam flow with real members remains J02-12.
Permissions, moderation, Leave, remote queue edits and audio are separate gates.
