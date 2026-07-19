# Room-2 ALF appendix: taxonomy1 compiler, ceiling gate, quorum aggregation

Gate companion to the frozen Room-2 contract (subconscious
docs/team-mode/room2-ceilings-taxonomy-quorum.md @ r4 fb7f49c7). Implementation
shapes for the ALF partition share (§8): the taxonomy1 compiler + Zone-1 stamp,
the ceiling gate at serve admission, and quorum aggregation on the org_ask
machine. Shapes only; no code lands before the gate verdict.

## A. taxonomy1 compiler

Home: `crates/alfonso-core/src/org/taxonomy.rs` (new, org module next to the
Room-1 slices). Pure and closed:

```rust
pub const TAXONOMY_VERSION: &str = "taxonomy1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LadderClass { Observe = 1, Compose = 2, MutateWorkspace = 3, MutateDurable = 4, PublishExternal = 5 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionStamp {
    pub class: LadderClass,
    pub credential: bool,
    pub financial: bool,
    // taxonomy_version rides the envelope next to the stamp, not inside it,
    // so the skew check compares two contract-pinned strings.
}

pub fn compile(tool_id: &str, validated_params: &Value) -> ActionStamp
```

Rules realized from the contract:
- Closed match on tool_id. The match arms are the authority; there is no
  registry lookup and no config. Unknown tool arm: `PublishExternal` +
  `credential: true` + `financial: true` (highest plausible in all three
  dimensions — strictly fail-closed, contract §1.2).
- `bash`-class tools compile through a command classifier reusing the
  InitiatorToolGate's classification approach (prefix/verb tables), with the
  same unknown-command fail-closed arm.
- Param splits: the params the compiler reads are the VALIDATED envelope params
  (post-schema), never raw client text. Flag examples: vault/grant/key params
  set `credential`; spend/provision params set `financial`.
- The compiler never reads content (file bodies, prompts, diffs) — tool plane
  only.

Stamping site: serve admission, the same function that stamps `target_agent`
(Room-1 Slice B). The dispatch envelope's `action_stamp` +
`taxonomy_version` fields are written there and ONLY there; a client-supplied
value in either field rejects pre-parse (deny-unknown-fields on the inbound
shape — the fields do not exist in the client-facing schema at all, which is
the unrepresentability bar: there is no representable way to submit a stamp).

## B. Ceiling gate

Home: serve admission, immediately after the Room-1 ceiling-relevant stamps,
before dispatch admission. Reads ONE artifact: the DecisionView snapshot Slice
B already resolves (which now carries the A2 claims `{ceiling_tier,
ceiling_flags, taxonomy_version, federation_exposure}` per CKCRED [#25]).

Predicate, three pure branches (contract §2.1/§2.5):

```rust
fn ceiling_admits(stamp: &ActionStamp, stamp_version: &str, view: &DecisionView) -> CeilingDecision {
    if stamp_version != view.taxonomy_version { return CeilingDecision::RefuseSkew }     // fail-closed
    if (stamp.class as u8) > view.ceiling_tier { return CeilingDecision::Park }
    if stamp.credential && view.credential_flag != Allowed { return CeilingDecision::Park }
    if stamp.financial && view.financial_flag != Allowed { return CeilingDecision::Park }
    CeilingDecision::Dispatch
}
```

- No second read: the ceiling facts come off the same snapshot the grant
  admission read (one-snapshot rule, R1 §8).
- Park routes to the org_ask machine (Slice C) with refusal reason
  `ceiling_exceeded` carrying `{stamp, ceiling_tier, flags}` — the ask body the
  approver sees names exactly what was clamped and why.
- The exposure predicate (FED's axis) runs beside this one; no ordering
  dependency, both read the same view.

## C. Quorum aggregation on org_ask

Home: `crates/alfonso-core-store/src/org_ask.rs` + the Slice C machine. No new
states (contract §4.1); three additive pieces:

1. PARK-TIME ELECTORATE RESOLUTION: at `park_org_ask` for a ceiling park, the
   caller resolves eligible approvers from the SAME bundle snapshot the gate
   read: members whose ceiling admits the stamp under the full conjunction
   (tier covers ladder class AND flags cover every set stamp bit — §4.2, the
   nobody-approves-what-they-cannot-perform rule), EXCLUDING the requesting
   subject (r4 F6: nobody sits in their own electorate). The frozen electorate
   is stored on the ask row as `electorate_json` (subjects + the
   bundle_version it resolved from, for post-hoc audit per CKCRED [#15](3));
   `quorum_n` stores the N rule outcome (1, or 2 when `stamp.financial`).
   Insufficient electorate (eligible_count < N, which subsumes empty) → the
   park REFUSES loudly with `quorum_electorate_insufficient` (r4 §5) — the ask
   row is never created.

2. APPROVAL ACCUMULATION: an approval is an ordinary ask-answer through
   `ingest_org_ask_answer`, extended with the `answered_by_subject` parameter I
   committed for WERNI's A2 — the subject check and the electorate check happen
   in the SAME transaction as the CAS: (a) subject ∈ frozen electorate, else
   typed annotation `approval_ineligible`, no state change; (b) duplicate
   subject approval → idempotent annotation, no double-count; (c) below-N
   qualifying approval → recorded as `approval` annotation, ask STAYS parked
   (this is the aggregation-above-answered_held: parked-with-k-approvals is
   still `parked`); (d) the Nth qualifying approval attempts the flip — see
   FLIP-TIME REVALIDATION below. Denials: any qualifying subject's denial
   resolves the ask denied immediately (deny short-circuits; quorum is for
   approval, not for denial symmetry — chair-adopted [#29], contract text in
   the r4 fold).

   FLIP-TIME REVALIDATION (r4 F1): the CAS-flip transaction re-reads the
   CURRENT bundle snapshot and revalidates EVERY counted approval against it
   (approver still a live member; ceiling still admits the stamp under the
   full conjunction). Lapsed approvals are discarded in-transaction as
   `approval_lapsed` annotations; the flip proceeds only if N approvals
   SURVIVE, else the ask stays parked with the surviving count. Rationale from
   the gate: ceiling/role/ACL changes are ordinary bundle_version bumps, not
   epoch bumps, so the park-time freeze alone would let a lapsed approver's
   authority survive to exercise time. The frozen set bounds the electorate
   ABOVE (no expansion after park); revalidation bounds it BELOW (no lapsed
   authority at exercise). One added snapshot read inside the CAS transaction
   Slice C already owns.

3. EPOCH KILL: unchanged Slice C semantics — membership_epoch bump kills
   parked AND answered_held asks including partial quorum (no resurrection,
   §4.1). Because the electorate is frozen at park, a role change that would
   EXPAND the electorate has no effect on in-flight asks, and one that shrinks
   authority arrives as an epoch bump and kills them — both directions safe.

## D. Conformance vectors owned by this share (§6 mapping)

- §6.1 compiler: every class reachable; unknown-tool → {PublishExternal,
  credential, financial}; bash split both branches; dual-stamp cases (low-tier
  + flag-allowed member vs PUBLISH+credential action parks on tier;
  high-tier + flag-ask member vs OBSERVE+credential action parks on flag).
- §6.2 gate: admit-at-boundary (class == tier); skew refusal; each flag
  branch both ways.
- §6.5 quorum: N=1 flip; N=2 financial accumulation; ineligible-approver
  annotation (the tier-5-with-credential:ask vector from [#18]); duplicate
  approval idempotency; park-time freeze vs later role change; epoch kill
  mid-vote; insufficient-electorate refusal (count < N, incl. empty);
  requester-excluded-from-own-electorate; flip-time revalidation (counted
  approval lapses on ceiling lowering between count and flip → approval_lapsed,
  flip refused at N-1 surviving; re-approval by a still-qualified subject then
  flips).

Vectors land as fixtures before any implementation claim (Room-1 discipline).
