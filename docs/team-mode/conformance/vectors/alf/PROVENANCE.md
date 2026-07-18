# ALF gate vectors — vendored provenance

Source of truth: alfonso
`crates/alfonso-core/tests/fixtures/room1-alf-gate-vectors.json`.
Vendored from commit: `27153b5` (joint-pass step 2).

Three gate vectors on the r1-alf-gate-<case> scheme, each resolving
against CKCRED's serving_envelopes.bundle (vectors/ckcred/):

- r1-alf-gate-bundle-predates-grant-bodies: refuses fail-closed on
  ABSENT agents[] (absent = unknown = refuse; a PRESENT empty array is
  the authorizes-nothing rule — a DIFFERENT arm, per the contract's
  empty-list-authorizes-nothing pin).
- r1-alf-gate-pre-enrollment-aud-null-refusal: aud binding is
  mint-time; a later org-daemon enrollment never retroactively
  validates a token minted in the aud-NULL era (CKCRED §3.5 Step-5).
- r1-alf-gate-three-tolerance-traversal-validator-half: terminates
  SUBC's r1-relay-unknown-field-tolerance expect chain (require-listed,
  reject-forbidden, ignore-unknown) with three negative controls incl.
  tolerance-bounded-by-size-budget.

Each carries its normative-clause citation + corpus_refs.

Refresh rule: re-vendor by commit hash + diff + a Room-1 channel
notice. alfonso is the authority; never edit here.
