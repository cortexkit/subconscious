# FED A4 vector set — vendored provenance

Source of truth: subc-federation
`test-vectors/rdv-wire/device-record.jsonl`.
Vendored from commit: `ac7c097` (§8 step 3, A4 layer shipped).

8 vectors on the r1-a4-<case> scheme (spec r4 §1.1):
r1-a4-valid, r1-a4-expired, r1-a4-wrong-account,
r1-a4-device-epoch-stale, r1-a4-equal-epoch-conflicting-payload,
r1-a4-wrong-cloud-key, r1-a4-wrong-cloud-key-expired (ORDER-PIN:
wrong-domain AND expired must fail BadSignature — proves
signature-before-temporal so no implementer leaks temporal info about
unauthenticated artifacts), r1-a4-ttl-pin (A4_TTL_MS=3600000, pinned
both sides).

Fed key domain: A4 vectors are signed with fed cloud-domain test keys,
never CKCRED's account JWKS — test keys never cross trust boundaries.

Refresh rule: re-vendor by commit hash + diff + a Room-1 channel
notice. subc-federation is the authority; never edit here.

(Superseded: the §5 emit-side slot was a named placeholder in
PLACEHOLDER.md until FED §8 step 5 landed. It has landed — see the
admission-facts-emit.jsonl section below — and PLACEHOLDER.md is gone.
This paragraph is kept because the sentence it replaces was still being
read as current: it sits above the section that contradicts it, and a
reader who stops at the first mention of the emit slot learns the wrong
thing.)

## admission-facts-emit.jsonl (§5 emit trio — corpus COMPLETE with this set)

SECOND COPY: these exact bytes are also vendored at
`clients/subc-client-swift/Tests/SubcFedTests/Fixtures/rdv-wire/admission-facts.jsonl`,
where `RdvVectorCurrencyTests` pins their digest. This copy has no digest
pin, so on a re-vendor the Swift copy fails loudly and this one changes
quietly — which makes the unpinned copy the one that silently becomes the
stale truth for whoever reads it. Re-vendor BOTH, or neither.

This copy is read by `admission_facts_relay_carries_vendored_packages_verbatim`
in `crates/subc-core/src/control.rs`, which drives the daemon's relay with
each package and asserts it arrives byte-identical. That test also guards
the property that makes these vectors useful as INPUT rather than as a
subject: at least one package must carry fields the daemon has no type for,
since a corpus flattened to commonly-modelled keys would leave the test
green while no longer able to distinguish a verbatim relay from a lossy one.

Vendored from subc-federation @ dad90c7 (`test-vectors/rdv-wire/admission-facts.jsonl`),
FED §8 step 5. Verified at vendor time: all three corpus_ids present
(r1-fed-emit-member-package-boundary, r1-fed-emit-service-package-no-member-fields,
r1-fed-emit-unknown-field-tolerance-half); declared byte lengths recomputed
independently over compact sorted serialization (member boundary package is
EXACTLY 4096 bytes — the emit half of r1-relay-size-boundary-accept's budget
edge); zero forbidden fields present in any package. The tolerance-half
vector encodes the emit terminus of the three-tolerance traversal: fed
emits ONLY schema fields (builders cannot produce forbidden fields by
construction), the unknown benign field is injected at the relay vector's
layer, subc relays it opaque, alfonso validates accept — the three-repo
traversal (fed-emit → subc-relay → alfonso-validate) is now proven with
every hop real. Refresh rule: re-vendor by commit hash + room notice, as
with every set.
