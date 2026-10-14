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

## 5. Gateway body convention

### 5.1 Body contract: MCP passthrough

The reverse-Request body IS an MCP request in miniature:
`{"method": "elicitation/create" | "sampling/createMessage" | "roots/list",
"params": {…}}` — the MCP shapes verbatim (rule 8: opaque to subc, a
module↔consumer contract). The gateway does zero translation of `params`; it
only wraps the method into a JSON-RPC server→client request on the shim's
stdio session. This keeps the gateway dumb, and it means the SAME body shape
works for Mode-2 plugin consumers (opencode/pi plugins answer it in-process,
no MCP host involved) — one asking convention across both fronts.

### 5.2 Fail-closed relay boundary

Before the MCP host completes `initialize`, or when it has not declared the requested elicitation, sampling or roots capability, the gateway returns an Error frame to the requesting module immediately without prompting the host. It also returns Error when the route no longer maps to a live shim session.

### 5.4 Conformance (gateway build gate)

- capability-declared host: aft reverse ask → host prompt → answer → aft
  receives it under its corr (end-to-end through real shim stdio).
- capability-absent host (and the attach→initialize window): aft receives a
  fast Error, never waits its timeout.
- CORR COLLISION/INTERLEAVE: with a forward tool
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
