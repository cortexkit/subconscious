# synthetic-input-review/v1

Review artifact for cerebellum's synthetic input plane (`computer.type`,
`computer.keys` via per-process event posting). Minted by subc after an
evidence review of the served wire path; cerebellum's `release-build.sh`
binds `CEREBELLUM_INPUT_REVIEW_REVISION` to this revision by name.

Bound in the same ceremony (one mint, no unbacked-gate window):

- `CEREBELLUM_INPUT_REVIEW_REVISION = synthetic-input-review/v1`
- `attendance_contract_owner = subc`
- `attendance_contract_revision = v1`

## What the review read (measured, not asserted)

1. **Served-path e2e**: full consent chain (elicitation-minted browser grant,
   leased compose target), type and keys delivered to an unfocused target
   (frontmost pid differed from target pid) with page-side readback agreeing.
   An operator-interference incident during an earlier run ("ou" typed
   mid-probe) was caught by the readback — evidence the comparison is real,
   not shaped.
2. **Rung and tier**: `pid_structural` served on the wire with no forfeiture
   reason; tier 3 (`owner_scoped + mutate + human_unobserved`) through the
   observation axis. The rung is backed by per-post kernel start-time
   re-verification atomically bound to each delivery; the staleness interval
   bound is inapplicable under atomic binding (no window exists), with
   `Atomic` **derived** from delivery-count witnesses (`posts * 2 ==
   generation verifications`) so a hoisted check or a zero-post sequence
   fails the claim by name.
3. **Refusal arms**: with the input review bound and attendance unbound, the
   dispatch gate refuses naming its missing inputs
   (`MissingExternalInput { gate: "attendance_contract_owner" / "_revision" }`).
4. **Intent journal**: journal rung matches the wire rung; typed text absent
   from decoded records, scanner proven alive on a control string.

## Scope and boundaries

- Covers `computer.type` and `computer.keys` only. `move`, `scroll`, `drag`
  remain withheld on their own merits: per-process posting carries keyboard
  events and not pointer events (measured with a global-dispatch control
  arm). Advertising any pointer capability requires a working delivery path
  and a **new revision of this review** — a new delivery mechanism is a new
  containment story.
- Unfocused delivery is unobservable **by construction**: the
  human-observed discount can never apply to it, in the code rather than in
  a remembered rule.
- `effect_status` honesty: the module does not claim post-condition
  observation for synthetic input (`effect_indeterminate`); the probe's CDP
  readback is the probe's, not the module's.

## Revision-advance policy

v2 is required when: a pointer-event delivery path is added; the containment
rung derivation changes mechanism; the attendance contract changes owner or
shape; or evidence shows intra-process misdelivery causing durable harm
(escalates the fronting question as a taxonomy change, per the fronting
ruling: fronting is recorded as its own audit fact and does not gate the
per-pid rung).
