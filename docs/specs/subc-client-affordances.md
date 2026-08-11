# Operational affordances a subc client must carry

Status: Built (this document describes shipped SDK behavior; the code is
authoritative — `crates/subc-client-rs/src/consumer.rs` for Rust,
`clients/subc-client/src/client.ts` for TypeScript. Where the two SDKs differ,
this document says so rather than averaging them.)

Audience: OWNERS OF HAND-ROLLED CLIENTS. Seven fleet modules speak the wire
directly on `subc-protocol`/`subc-transport` (aft, broca, engram, astrocyte,
cerebellum, insula, claustrum) and never execute the SDK code where these
affordances live. The SDKs arrived at each behavior by decision; a hand-rolled
client arrives at whatever its author reached for. The motivating failure
shape: a client that retries a wrong module id without a bound and without
ever printing the id it is dialing presents as an outage of the TARGET module,
indefinitely, with the client's own health green throughout.

This is a list of CONCERNS, not constants. The constants are readable in the
SDKs and are situational — a consumer may deliberately run a shorter call
timeout than the SDK default because its own outer loop carries the retry
budget. The VALUE differs; the CONCERN is identical. A hand-rolled client does
not know what it is missing, which is why the list is worth more than the code.

For each item: what the SDKs do, and what breaks without it.

## 1. Route-open retry is deadline-bounded, and the failure must name the target

The retryable code set for `route.open`, identical in both SDKs (Rust
`is_retryable_route_open_code`, TS `isRetryableRouteOpenCode` — each pinned by
its own unit test; there is no cross-SDK conformance test for this today):

    unknown_module · module_reloading · target_unavailable · module_timeout

Everything else (`bad_consumer_identity`, `invalid_project_root`, …) is
permanent and fails immediately. Retries stop at a deadline — TS
`ROUTE_OPEN_RETRY_DEADLINE_MS` = 30s inside managed `call()`; Rust's
`route_retry_deadline` defaults to 30s and is configurable per call, with an
optional `max_attempts` that can stop earlier. Scope note: the retry loop
lives in the MANAGED call path in both SDKs; TS `routeOpen()` called directly
performs one RPC and does not retry.

Without the bound: `unknown_module` is AMBIGUOUS between "restarting, will
return" and "never registered / wrong id". An unbounded retry loop converts a
typo'd module id into an eternal quiet retry — the caller waits forever while
every gauge reads healthy.

On the naming half, the precise failure mode: THE DAEMON'S REJECTION PAYLOAD
ALREADY NAMES THE MODULE ID. A client that WRAPS the daemon's message inherits
the naming for free and is compliant; the failure mode is specifically a
client that BUILDS ITS OWN TERMINAL MESSAGE and drops the payload. Testable
by asking whether the daemon's own bytes survive to the caller. (One
hand-rolled owner audited this item, wrote an id-injection fix, and the
mutation test showed the fix redundant — the propagated payload already
carried it. Wrap, don't substitute.) Rust's deadline expiry today reports
`"retry deadline elapsed"` without re-stating the id at the outermost layer —
if you substitute messages anywhere, re-attach the target.

Note `module_warming` appears in `docs/ARCHITECTURE.md` as a distinct
post-respawn state; as of this writing NO shipped code emits or matches it —
do not build handling for it from the architecture doc alone.

## 2. unknown_channel means evict-and-reopen-once, not retry and not fail

The daemon emits `unknown_channel` only PRE-DELIVERY (the frame never reached
the module), so re-issuing the request is always safe — but the route is gone.
Both SDKs' MANAGED call paths: evict the cached channel, re-open the route,
retry the request ONCE in place. (Low-level single-shot request APIs do not do
this; if you hand-roll at that layer, the recovery is yours.) Retrying without
re-opening loops on the same dead channel; failing without re-opening turns
every daemon-side route release into a caller-visible error.

## 3. A mid-request GOODBYE is outcome-UNKNOWN, never not-sent

"Mid-request" precisely: the request bytes were accepted by the local socket
writer, and no terminal frame for its corr has arrived. From that point a
route teardown (GOODBYE, or connection loss) leaves the outcome unknowable —
queued-to-socket does not prove the daemon received it, and daemon-received
does not prove the module ran it, so the classification is conservative in
the caller's direction. Three terminal classes, which hand-rolled clients
must keep distinct:

    not_sent          — provably never left this process (safe to retry
                        ANYTHING, including mutations)
    outcome_unknown   — accepted by the writer, no terminal frame observed
                        (retry only idempotent operations; a mutation retry
                        may double-apply)
    failed            — a terminal Error frame arrived, or the request was
                        refused before send for a permanent reason (the
                        outcome is KNOWN)

Collapsing outcome_unknown into failure invites blind retry of mutations —
the false-failure class: a terminal that under-reports success is worse than
one that under-reports failure, because failure invites investigation while
false-failure invites the retry that double-applies.

## 4. Every pre-send wait carries a deadline

Route flow-control credits, route-open single-flight, reconnect waits, writer
capacity: the Rust SDK bounds ALL of them and aborts expired requests as
`not_sent`. A missing bound on any one converts module backpressure into an
unbounded caller hang — and the hang reports as the CALLER's unresponsiveness,
one component away from the cause.

## 5. Reconnects preserve identity and treat auth failure as transient

The two SDKs differ in WHEN routes come back, and both shapes are valid:

- Rust clears cached routes on reconnect and re-opens LAZILY on the next call.
- TS proactively re-opens cached routes after reconnecting.

What must hold in either shape: the re-open carries the ORIGINAL
`consumer_identity` (a route re-opened without it downgrades the module's view
of the caller from the spawn-attested `Reserved{module_id}` principal to
`Direct` — providers enforcing caller-type security then refuse, and the
refusal reads as their bug). TS additionally preserves declared
`consumer_capabilities` on the session; if your client declares capabilities,
re-declare them on re-open. Auth failures during reconnect are TRANSIENT (the
daemon may be mid-key-rotation; re-read the connection file on the next
attempt). Reconnect attempts are generation-fenced in both SDKs so a stale
reconnect task cannot replace a newer connection.

## 6. Deadline expiry under load is arbitrated, not acted on

(TS managed `call()` path.) When a request deadline expires, the event loop
itself may be the late party. The SDK yields a check phase and a bounded grace
window — granted when the socket has buffered bytes or reads are in flight —
and only if no terminal arrives does it settle, as outcome_unknown with
`deadline_exceeded_no_drop_observed`, KEEPING the connection. Tearing down the
socket on a timer that the event loop starved converts CPU pressure into a
reconnect storm. Single-threaded hand-rolled clients (and any runtime that can
starve its own timers) need this arbitration; a multi-threaded Rust client
generally does not.

## 7. Frame-dispatch drops must be observable

A client's reader loop drops frames in several classes when correct: frames
for an epoch it no longer holds, and terminal frames whose (channel, epoch,
corr) matches no pending entry. The drops are right; their INVISIBILITY is
the defect. The SDKs are only partway there themselves — TS counts
stale-epoch ingress (`ingressEpochDropCount`) but only debug-logs unmatched
terminals; neither SDK counts every class. Hand-rolled clients should count
BOTH classes: a drop class that only increments a debug log is invisible
until someone asks, and "delivered zero frames" is otherwise indistinguishable
from "peer sent nothing" — two states, one output, and the confusion
reliably sends an investigation to the wrong component.

Count the two classes SEPARATELY: folding them into one number lets a
burst of the ordinary kind (replies racing a reconnect — expected during a
daemon restart) hide the kind that is never expected. Keep them cumulative
across reconnects, since a reconnect is exactly the event whose evidence
you want afterwards. And PUBLISHING IS THE HALF THAT IS EASY TO SKIP: a
counter no surface exposes is the same invisibility this item exists to
fix, wearing a different coat (one implementation's dead-code lint
rejected exactly this — the correct verdict for the wrong reason). The
satisfiable-vacuously form is "counted"; the real requirement is counted
AND readable from outside the process.

Field-shape rule when surfacing the counters (in a health payload or
anywhere): EMIT ZERO WHEN ZERO IS A STATE; OMIT WHEN ZERO IS A LIE. A COUNT
has a meaningful zero ("no drops occurred"), and emitting it always makes
closure a positive observation — an absent field then unambiguously means
the instrument is gone rather than idle. An AGE has no meaningful zero
(`vault_ok_age_s: 0` reads "succeeded just now", the opposite of "never
succeeded") and must be omitted until it has a value. Copying one
convention across both field kinds gets one of them wrong, silently, in
both directions.

## 8. Consumer identity comes from the environment, not from configuration

`SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` are injected by the supervisor at
spawn; SDKs attach them automatically to `route.open`. A hand-rolled client
that hardcodes its OWN identity (rather than reading `SUBC_MODULE_ID`) breaks
on a daemon-side module rename — the supervisor's env override is what lets a
module be renamed in `subc.jsonc` without a rebuild — and one that omits the
nonce silently downgrades itself to an unattested `Direct` principal.

## 9. Strict integer parsing at the wire boundary

Channel and epoch values are validated as integers in range; out-of-range JSON
bytes are REJECTED rather than coerced (silent modulo truncation aliases one
route onto another; an aliased channel is not an error you observe — it is a
frame delivered to the wrong route). The Rust connection-file parser enforces
integer types and port range; the TS parser is looser today (it accepts any JS
number for the port). Hand-rolled clients should hold the strict line at both
boundaries rather than copying the weaker of the two.

## 10. Route handles are connection-scoped

The SDKs bind every route handle to a private per-connection token; a handle
minted on connection N cannot act on connection N+1 even when the daemon
reuses the same channel and epoch numbers. Hand-rolled equivalent: on ANY
reconnect, invalidate every held route handle and re-open. Reusing a numeric
(channel, epoch) across connections eventually delivers frames onto an
unrelated route — the failure is rare, delayed, and looks like data
corruption.

## 11. Bind-root identity is canonicalized, and a directory rename silently re-partitions it

Route binding canonicalizes the project root (realpath at the daemon), and
anything a client DERIVES from that root — storage keys, WAL hashes, session
identity — is keyed by the canonical path, not the spelled one. Two traps:

- A compatibility symlink does NOT preserve derived identity: canonicalization
  is precisely the step that follows the symlink, so a folder rename
  re-partitions every path-derived key even when the old path still resolves.
  The failure has NO signal: valid root, successful bind, empty history —
  indistinguishable from a genuinely new session. (A census after one rename
  found 65 sessions stranded across two stores, 41 of them unnoticed by their
  own client.)
- When computing any path-derived key, resolve the root the way the daemon
  will FIRST (realpath, then derive) — `/tmp` is a symlink on macOS, and a
  path that looks canonical to a human is not necessarily canonical to the
  resolver.

Discovery predicate for the stranded case: compare `realpath(embedded_root)`
against the embedded root in stored records — valid ONLY while the old path
still resolves, so the audit window closes when someone tidies the symlink.
Recovery is a journaled re-key (see `docs/module-rename-runbook.md`, third
store class).

## The checklist form

A hand-rolled client audits itself by answering these; every "no" is a latent
incident with the failure mode attached above.

1. Is route-open retry bounded, on exactly the four retryable codes, and does
   the terminal failure name the module id?
2. Does unknown_channel evict + reopen + retry once?
3. Are not_sent / outcome_unknown / failed distinct terminals — with automatic
   mutation retry permitted on not_sent (and pre-delivery unknown_channel),
   and NEVER on outcome_unknown?
4. Does every pre-send wait carry a deadline?
5. Do re-opens after reconnect carry identity (and any declared capabilities),
   with auth failure treated as transient?
6. Is deadline expiry arbitrated against runtime starvation before the
   connection is torn down?
7. Are both silent drop classes (stale-epoch, unmatched-terminal) counted
   somewhere a human will read?
8. Do the client's own module id + launch nonce come from the environment?
9. Are channel/epoch (and the connection file) parsed strictly as in-range
   integers?
10. Are route handles invalidated across reconnects?
11. Are path-derived keys computed from the realpath'd root, and is there a
    plan for what a project-root rename does to them?
