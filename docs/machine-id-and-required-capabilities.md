# The daemon owns the machine id, and required capabilities are enforced at routing

The daemon mints and serves one id for the machine, and refuses to route to a
module whose required capabilities are not provided. An earlier revision of
[`capability-grammar.md`](capability-grammar.md) only reported unmet required
capabilities; routing now enforces them.

## 1. The machine id

**What.** 16 random bytes, rendered as 32 lowercase hex characters. Opaque: no
structure, no derivation from a key, a hostname or any host fact.

**A name, never an authority.** Nothing may admit a peer, grant trust or skip
a check because two messages carry the same machine id. Authority stays on keys
(callosum's roster row, the vault). The id deliberately outlives a key rotation,
so treating it as identity would let a revoked key's history vouch for its
replacement. This rule goes in the doc comment on every type that carries it.

**Where it lives.** `<data home>/cortexkit/machine-id`, one line, written once
through a temporary file and a rename, never rewritten by the daemon. The daemon
mints it at startup if the file is absent, before it accepts a connection, so
every module that registers sees the same value. A present file that does not
parse as 32 lowercase hex characters stops the daemon at boot with a named
error: a corrupt identity is not silently replaced.

**How modules learn it.** A new optional field on the registration reply,
`ModuleHelloAckBody.machine_id: Option<String>` (`serde(default)`, omitted when
absent). Also on `server.describe` and in `ck daemon`. A module built against an
older daemon sees nothing and must treat that as "daemon predates the machine
id", never as "no machine".

**The file is the authority, readable without the daemon.** A tool that runs
while the daemon is down (callosum's `trust import`, which runs with the callosum
module stopped) reads `<data home>/cortexkit/machine-id` directly, never the
daemon's control API. The running daemon's copy equals the file except between
an `adopt` and the next daemon start.

**Restore, and the order that makes it safe.** Restoring a machine means
restoring its id before anything keys state on it:

1. `ck machine adopt <id>` with the daemon stopped (or before its first start on
   a rebuilt machine). `adopt` is an operator command, not a module operation,
   because it changes the identity every module on the machine reports. It
   writes the file and never touches a running daemon.
2. `ck callosum trust import`. Callosum's trust export records the id as data.
   The import compares it with the file: equal, proceed; different, refuse and
   name step 1; no file, refuse and name step 1 (never skip the comparison,
   which would let a restore onto the wrong machine through silently); the
   recorded id absent (an export older than this change), proceed without
   comparing and keep nothing to compare later.
3. Start the daemon.

**The wrong order, and what it costs.** A rebuilt machine whose daemon starts
before step 1 mints a fresh id, and modules may create state keyed on it (a
ck-bus account's streams, prefrontal's local machine binding). A later `adopt`
changes the id at the next daemon start. So a module that keys durable state on
the id must handle registration with an id different from the one it last saw,
and must say in its own design what it does: ck-bus starts a new account and
treats the old one's streams as orphans to collect; prefrontal's rule is
to write. `ck setup` on a fresh machine asks whether it is a new machine or a
restore before the daemon's first start, so the safe order is the default path.

**Clone detection stays callosum's.** Two machines restored from one backup
carry one machine id with two different transport keys. Only callosum sees both
keys, so its §2.5 rule stands unchanged, keyed on the daemon's id instead of a
callosum-minted one.

**Phones** run no daemon and have no machine id (callosum r2: a phone never
mints or announces one).

## 2. Required capabilities hold a module not-ready

**Today.** A module declares `capabilities.requires` with `need: required` or
`need: optional`. The daemon evaluates each requirement as `provided`,
`pending` or `never_provided` and reports a missing required provider loudly,
but routing is unaffected. During the daemon's own ordered shutdown, when
providers deregister as their modules exit, requirement changes log at INFO
with `cause=daemon_shutdown` instead.

**New rule.** A module's effective readiness is its declared readiness (rung 2)
AND every one of its `required` capabilities being `provided`. While any is
`pending` or `never_provided`, `route.open` to that module is refused with the
existing retryable `module_warming`, `detail.reason =
"required_capability_unprovided"` and `detail.capability` naming the first
unprovided capability in lexicographic order, a distinct log field, and a
distinct counter key (`module_warming_required_capability_unprovided`).
Callers retry inside their deadline exactly as they do for any warming module,
so no SDK changes.

What it does not do, deliberately:

- **No spawn ordering and no boot block.** The module starts, registers and
  can make its own calls. Ordering is a promise that cannot be kept after a
  provider crashes, and a boot refusal stops a whole machine, including the
  tools needed to fix its configuration.
- **No teardown of existing routes.** When a provider goes away at runtime, new
  opens to the consumer are refused until it returns; routes already bound stay
  bound and the consumer answers them as it can.
- **No deadlock.** A capability counts as `provided` when its claimant has
  registered, not when the claimant is ready. So two modules that require each
  other's capabilities are both provided as soon as both register.

Optional needs are unchanged: silent, and the consumer degrades by declaration.
