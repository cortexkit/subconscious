# FED emit-side vector slot — named placeholder (A4 now REAL)

A4 landed real @ subc-federation ac7c097 → device-record.jsonl (see
PROVENANCE.md). Only the §5 emit-side slot remains a placeholder; it
is sequenced at FED §8 step 5 (emission against SUBC's shipped relay),
re-run as a corpus refresh when it lands.

## Emit-side package slot (spec r4 §5)

- r1-fed-emit-member-boundary (member package, all required fields, at
  the 4096-byte size boundary — the emit half of
  r1-relay-size-boundary-accept)
- r1-fed-emit-service-no-member-fields (service package asserting no
  member fields — the emit half of r1-relay-service-forbidden-subject)
- The emit half of the three-tolerance traversal
  (r1-relay-unknown-field-tolerance): fed emits a package with an
  unknown benign field; subc relays opaque; alfonso validates accept.
