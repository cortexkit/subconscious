# CKCRED fixture set — vendored provenance

Source of truth: cortexkit-account
`workers/account/test/fixtures/room1-contract-samples.json` (+ the
mint script for regeneration provenance).

Vendored from commit: `ac011eb` (the frozen GATED-v5 spec commit).
Fixture BYTES last changed at `ba5021f` (r3 fold: header typ:"JWT",
the 33 vector_ids, serving_envelopes samples); r4/r5 were spec-prose
only (server-internal D1 columns that never reach a wire artifact), so
ac011eb's fixture content is byte-identical to ba5021f's.

Contract label inside the set: v7.3 @ f02d9b2f. 33 vector ids on the
r1-<family>-<case> scheme. `serving_envelopes.bundle` is THE single
bundle artifact all four seats' vectors resolve against (one snapshot,
three consumers, zero side-fetches — made literal).

Refresh rule: re-vendor by commit hash + diff + a Room-1 channel
notice. Never edit these files in place here; the account repo is the
authority.
