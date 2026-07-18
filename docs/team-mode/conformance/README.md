# Room-1 Conformance Corpus

One artifact set, four consumers (CKCRED, FED, ALF, SUBC), zero drift
by construction: every implementation suite cites vectors from this
corpus by stable id instead of minting private fixtures for shared
contract surfaces.

Normative authority: the frozen contract
`../room1-org-grant-acting-for-contract.md` (v7.3 @ f02d9b2f, plus
recorded amendments). Every vector carries the contract clause it
proves. On any divergence between a vector and the contract text, the
contract wins and the vector is a defect.

## Vector identity

Ids follow CKCRED's `r1-<family>-<case>` scheme corpus-wide (e.g.
`r1-a3-intent-collision`, `r1-relay-unknown-field-tolerance`).
Failures name the vector id, never a file+index. Ids are stable
forever; a changed vector keeps its id only if its meaning is
unchanged (else it is a new id and the old one is retired with a
note).

## Sources (vendored by commit hash, refresh = diff + Room-1 notice)

- CKCRED artifact fixtures: cortexkit-account
  `workers/account/test/fixtures/` (the normative schema source; the
  serving_envelopes.bundle sample is THE single bundle artifact every
  side reads).
- FED A4 + emit-side package vectors: subc-federation (fed key domain;
  test keys never cross trust boundaries).
- ALF gate vectors: alfonso (fail-closed bundle-predates-grant-bodies,
  pre-enrollment aud-NULL-era refusal, three-tolerance traversal
  host).
- SUBC relay vectors: this directory.

## Fleet rules proven by vectors here

1. JWT artifact verification is PRESENT-REQUIRED-CLAIMS, never
   reject-unknown-claims. The discriminator is typ+aud+signature.
   (`r1-jwt-benign-extra-claim` — an artifact with an extra additive
   claim MUST verify.)
2. Package validation ignores unknown NON-FORBIDDEN fields (additive
   evolution); forbidden fields reject by NAME, by presence, value
   irrelevant. A deny-unknown-fields validator is a contract
   violation that reads as rigor.
   (`r1-relay-unknown-field-tolerance` — a member package with an
   unknown benign field MUST validate end-to-end: fed emit → subc
   relay → alfonso validate.)
3. Header typ is "JWT" always; the PAYLOAD typ claim discriminates.
   (`r1-jwt-payload-typ-discriminates`.)
