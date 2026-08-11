# SubcFed rekey/drain entrance — design constraints

Status: Not built. The subsystem below exists and is complete except its
entrance; this file pins the contract the entrance must honor, written the
day the exposure was sized rather than the day someone builds it.

## The finding (2026-07-25, re-verified at HEAD 2026-08-11 by alfonso-ios)

- `beginDrain` is the sole entry to `phase = .draining` and has zero
  production callers; `role = .replacement` is never set.
- `FedSessionEngine` handles `fed_rekey_needed` with the "do not treat as
  failure" half only: the peer says rekey-needed, the client returns bare,
  and the session continues past its rekey point.
- alfonso-ios never tears down a session on suspend: `disconnect()` has two
  callers (dev-settings edit, WiFi/cellular hop). A phone foregrounded on
  stable WiFi holds ONE KEY SET INDEFINITELY — the desk-overnight case, on
  the device with the weakest physical security.
- Session lifetime CANNOT be sized from the audit ledger: `incarnation` is
  minted per durable state file (`FedGlobalReservationState.mintFresh`),
  not per session. Three days of traffic read as one 64.9-hour incarnation.
  A naive ledger read produces a nonsense exposure number in the alarming
  direction; do not quote it.

## Contract for the entrance (agreed with alfonso-ios, 2026-08-11)

1. MAKE-BEFORE-BREAK LIVES IN SUBCFED, not in consumers. All the state is
   here; N consumers each reimplementing a silent security-shaped
   obligation is the wrong shape, and getting it wrong is invisible.
2. SEAM REQUIREMENT (the consumer's one line): a rekey or drain MUST NOT
   surface as `disconnected` in `FedConnectionState`. Transport bars render
   `disconnected` as "Lost the connection to your Mac"; a healthy rotation
   presenting as a fault is the waiting-is-not-failing defect one layer
   down. Invisible is fine. Error-looking is not. If an intermediate state
   is exposed at all it is a distinct `rekeying` value, never `disconnected`.
   Refinement (alfonso-ios, pinned 2026-08-11): "invisible" applies to
   PRESENTATION only. The consumer's two mappings deliberately treat a new
   case oppositely — the status bar falls through to nothing (tested:
   `anUnnamedStateLeavesTheBarUntouched`, mutation-proved), while the reuse
   rule (`isReusable`) is an exhaustive switch so a new case is a COMPILE
   ERROR forcing a deliberate liveness classification. "May a draining
   session still serve calls" must never be decided by a default arm.
   THEREFORE (mechanism corrected by alfonso-ios 2026-08-11 — the first
   wording said "frozen enum", which names the WRONG mechanism): the
   package MUST CONTINUE TO BE CONSUMED AS A SOURCE DEPENDENCY. `@frozen`
   is not a live concept here (no library evolution, zero @frozen in
   Sources/); exhaustiveness is enforced today BECAUSE the package ships
   as source — proof by construction: the consumer's `isReusable` names
   all nine cases with no default arm and compiles clean. The property
   ("a new case must break the consumer's build") is lost by enabling
   library evolution or shipping a prebuilt/binary framework — EITHER IS
   A BREAKING CHANGE TO THIS CONTRACT. Note the trap the old wording
   invited: "must remain frozen" read against a package with no @frozen
   anywhere could be "fixed" by adding @frozen while enabling library
   evolution — reading as compliance while silently changing nothing
   about the real hazard.
3. The old session drains only after the replacement serves: the drain
   phase exists for the overlap, which is the make-before-break property.
4. Rekey triggers: `fed_rekey_needed` from the peer is the demand-driven
   entrance. Any time/byte-count-driven local trigger is a separate,
   later decision; do not conflate the two in one slice.

## Dispatch posture

Noise-path work is security-critical and requires explicit user sign-off
before a worker slice is dispatched (standing posture since 2026-07-22).
