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

The §5 emit-side slot (r1-fed-emit-*) remains a named placeholder in
PLACEHOLDER.md until FED §8 step 5 (emission against SUBC's shipped
relay) lands.
