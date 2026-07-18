# FED vector slots — named placeholders

FED's A4 layer is in implementation (subc-federation, §8 step 3);
their §5 emit-side vectors are sequenced at step 5 (they consume
SUBC's shipped relay). Per the joint-pass staging rule, these slots
are NAMED now and re-run against real vectors when FED posts the
landing commits — that re-run is a corpus refresh, not a phase
reopening.

## A4 slot (r1-a4-<case>, spec r4 §1.1)

- r1-a4-valid
- r1-a4-expired
- r1-a4-wrong-account
- r1-a4-epoch-stale
- r1-a4-equal-epoch-conflict
- r1-a4-wrong-cloud-key (cross-domain negative)
- r1-a4-ttl-pin

## Emit-side package slot (spec r4 §5)

- r1-fed-emit-member-boundary (member package, all required fields, at
  the 4096-byte size boundary — pins the emit half of
  r1-relay-size-boundary-accept)
- r1-fed-emit-service-no-member-fields (service package asserting no
  member fields — the emit half of r1-relay-service-forbidden-subject)
- The emit half of the three-tolerance traversal
  (r1-relay-unknown-field-tolerance): fed emits a package carrying an
  unknown benign field; subc relays opaque; alfonso validates accept.

Fed key domain: test keys never cross trust boundaries — A4 vectors
are signed with fed-domain test keys, never CKCRED's test JWKS.
