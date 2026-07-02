# subc Reverse-Request Lane (server-initiated requests on a route channel)

Status: DRAFT contract (the "(A) bless" step of gateway primitive #3 —
elicitation). Blesses behavior the router already exhibits (source-verified);
adds NO subc-core production changes. First consumer: AFT
bash-permission-over-elicitation.

## 1. Contract

1. A module MAY send a `Request` frame on a bound route channel (module→client
   direction). The daemon forwards it like any data frame: channel-rewrite,
   best-effort `try_send`, zero body inspection.
2. Corr namespaces are **per-originator**: each endpoint mints corrs for its own
   requests and matches incoming `Response` frames against its own outstanding
   table. Direction is disambiguated by frame TYPE (an incoming `Request` means
   "the peer is asking me"), so numerically-equal corrs across directions never
   collide.
3. The consumer answers with a `Response` (or `Error`) frame on the same
   channel + the module's corr.
4. **Flow-control:** reverse Requests take NO credit (credit is acquired only on
   client→module `Request`). The forward tool-call's credit stays held for the
   full duration including any mid-call ask — a Serial module mid-elicitation is
   correctly "busy", never deadlocked.
5. **Reliability floor (normative, module-side):** the lane is best-effort.
   A module awaiting a reverse answer MUST run a timeout and **fail closed**
   (deny the pending operation) on: no answer, undeliverable send, consumer
   disconnect, or route teardown. This floor is mandatory regardless of any
   future reliable sub-lane — guaranteed delivery never guarantees a human
   answers.
6. **Settlement:** a `Cancel` for the enclosing forward call, or a route
   `GOODBYE`/teardown, settles any outstanding reverse request — the module
   resolves it as cancelled/denied and never hangs. Released-channel behavior is
   direction-split (standing router behavior, empirically pinned): a frame sent
   by the MODULE to a released channel is dropped silently; a CLIENT frame on a
   released/unknown channel is answered with `Error{unknown_channel}` (useful
   client diagnostics; our TS closeRoute path relies on it). A late client
   answer to a torn-down reverse request therefore yields a client-visible
   `unknown_channel` Error carrying the module's corr — benign under rule 2's
   per-originator matching (it matches nothing in the client's outstanding
   table). Either way the module receives nothing and resolves via rule 5.
7. A consumer that does not support reverse requests SHOULD answer with an
   `Error` frame (fast fail-closed); one that silently drops still resolves via
   the module's timeout (rule 5).
8. Body bytes are opaque to subc (thin-core). The elicitation JSON shape is a
   module↔consumer contract; for the MCP gateway it is the MCP
   `elicitation/create` / `sampling/createMessage` / `roots/list` passthrough.

## 2. What subc-core does NOT add

No reliable reverse sub-lane, no anti-flood window, no reverse-request tracking
in the router. Those are phase-2 LIVENESS upgrades (fewer spurious denials), not
correctness requirements — the module-side floor (rule 5) is the correctness
mechanism. The router stays a dumb splice.

## 3. Conformance tests (subc-core, lock the emergent behavior)

- module→client `Request` on a bound route forwards with the channel rewritten;
  client `Response` (same corr) routes back to the module.
- No credit interaction: a Serial (window=1) module with its forward request
  in-flight can send a reverse `Request` and receive the answer (no deadlock);
  the forward credit releases only on the module's terminal.
- Teardown: after route `GOODBYE`, a late consumer `Response` to the released
  channel is dropped silently; nothing crashes or misroutes.
- Interleave: reverse `Request` and forward traffic on other channels do not
  cross-contaminate (corr per-originator, frame-type disambiguation).

## 4. Degrade model (unified, for the gateway + AFT)

RESTRICT = floor, ELICITATION-PROMPT = ceiling; a capability handshake picks
which. Upstream host lacks elicitation → the gateway fails the reverse request
cleanly (module fail-closes per rule 5) → AFT falls back to forced-restrict /
deny-out-of-root (today's behavior). Same fallback path everywhere.

## 5. The gateway relay (subc-mcp module — DESIGN v2, Oracle-revised, the
unbuilt piece)

The subc-mcp module is the consumer on the aft route, so a reverse `Request`
from aft arrives on its `SubcClient` route channel. The relay turns it into an
MCP server→client request to the upstream host and routes the answer back.

STRUCTURAL GROUND TRUTH (Oracle-verified at source, v1's prose was wrong on
two counts): the shim is a dumb byte pipe — MCP JSON-RPC terminates in the
MODULE's rmcp server (`serve_server` over the shim transport), and rmcp's
`Peer<RoleServer>::send_request` makes server→client requests feasible from
module-side code. BUT nothing dispatches an unsolicited reverse Request today:
the shared subc reader loop matches frames only by (channel, corr) against
module-originated pending calls — a reverse Request with a fresh corr is
DROPPED, and one whose corr numerically equals an in-flight forward call's
would be misdelivered into that call (a rule-2 violation). And the module's
session state maps module_id→route per session, but there is NO reverse
route→session registry. The build therefore adds two real pieces:

- **Reverse dispatch in the subc reader:** frames are dispatched by TYPE
  first — an incoming `Request` is a reverse ask (never matched against the
  outbound pending table), routed to the relay; everything else keeps today's
  (channel, corr) matching. This implements rule 2's per-originator split at
  the one place it was missing.
- **A relay registry:** `route_channel → shim session` (written when the
  module opens a route for a session, removed on route/session teardown) plus
  a pending-relay table `(route_channel, reverse_corr) → pending MCP request
  handle`. Duplicate policy: a second reverse Request with the SAME
  (route, corr) while one is pending is IGNORED (no second host prompt, the
  original stays authoritative).

### 5.1 Body contract: MCP passthrough

The reverse-Request body IS an MCP request in miniature:
`{"method": "elicitation/create" | "sampling/createMessage" | "roots/list",
"params": {…}}` — the MCP shapes verbatim (rule 8: opaque to subc, a
module↔consumer contract). The gateway does zero translation of `params`; it
only wraps the method into a JSON-RPC server→client request on the shim's
stdio session. This keeps the gateway dumb, and it means the SAME body shape
works for Mode-2 plugin consumers (opencode/pi plugins answer it in-process,
no MCP host involved) — one asking convention across both fronts.

### 5.2 Relay mechanics

1. **Capability gate (recorded post-initialize, not at bind):** MCP
   `elicitation`/`sampling`/`roots` are CLIENT capabilities declared in the
   host's `initialize` — which happens AFTER the module opens the session's
   routes (attach precedes the MCP handshake). The gateway records the
   session's capabilities from rmcp's peer info when initialize completes; a
   reverse Request arriving in the attach→initialize window is answered
   fail-closed (Error) like the capability-absent case.
2. **Reverse Request arrives** on a route channel → the reader's type-first
   dispatch hands it to the relay → the relay resolves the owning shim session
   via the route→session registry (stale/missing entry → immediate Error).
   - Host lacks the capability → answer the reverse Request immediately with
     an `Error` frame (rule 7's fast fail-closed) — aft falls back to
     restrict without waiting out its timeout.
   - Host declared it → forward as a JSON-RPC request (gateway-minted JSON-RPC
     id, mapped to the pending route corr) through the shim's stdio.
3. **Host answers** → the gateway matches the JSON-RPC id → sends the result
   as a `Response` frame on the route channel with the module's corr.
   A JSON-RPC error → an `Error` frame (aft treats as deny).
4. **Settlement (complete enumeration):** each of these removes the pending
   relay entry exactly once and resolves the module side at most once —
   (a) host answers → Response/Error to the module; (b) shim disconnect →
   Error to the module + cancel the outstanding rmcp request; (c) route
   GOODBYE/teardown → drop the entry (the module side is gone; a late host
   answer then matches nothing and is dropped); (d) a `Cancel` frame for the
   enclosing forward call on that route → Error to the module + cancel the
   host prompt (the ask's reason is gone — don't leave a zombie prompt up).
   Pending entries are bounded per shim session; overflow answers `Error`
   immediately (anti-flood without any subc-core involvement).
5. **Relay TTL (leak backstop), no timeout on the human's DECISION semantics:**
   aft owns the ask timeout (rule 5, module-side floor) — the gateway never
   decides the ask. But an entry pinned by a host that never answers on a
   long-lived route is a slow leak, so entries carry a generous TTL (default
   comfortably above any module ask timeout, e.g. 10min): expiry drops the
   entry + cancels the rmcp request; the module has long since fail-closed.

### 5.3 What the gateway does NOT do

No policy on the ask content (it can't read it — opaque passthrough), no
retry, no re-prompt, no answer caching. One ask = one relay = one answer or
one settlement. Reliability upgrades stay phase-2 exactly as §2.

### 5.4 Conformance (gateway build gate)

- capability-declared host: aft reverse ask → host prompt → answer → aft
  receives it under its corr (end-to-end through real shim stdio).
- capability-absent host (and the attach→initialize window): aft receives a
  fast Error, never waits its timeout.
- CORR COLLISION/INTERLEAVE (the Oracle's required gate): with a forward tool
  call outstanding on a route, the provider sends a reverse Request whose corr
  NUMERICALLY EQUALS the forward call's corr — the type-first dispatch routes
  it to the relay, the forward call completes unpoisoned, and the reverse
  answer returns under the module's corr.
- duplicate reverse Request (same route+corr) while pending: ignored, no
  second host prompt, the original resolves normally.
- shim death mid-ask: pending relay settles as Error; aft resolves; no leak.
- Cancel of the enclosing forward call mid-ask: entry removed, host prompt
  cancelled, module receives Error.
- bounded pending: the N+1th concurrent ask on one session answers Error.
- TTL expiry: an unanswered entry past TTL is dropped + rmcp request
  cancelled, without disturbing other pending relays.
- The AFT bash-permission flow is the end-to-end proof (AFT wires it as its
  first consumer; retires its in-band permission_required hack and flips
  forced-restrict from ceiling to pre-elicitation floor).
