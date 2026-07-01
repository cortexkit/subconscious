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
