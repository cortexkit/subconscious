# Foundation F-PROV disposition mapping

Source: `nats-message-plane-foundation.md` at prefrontal commit
`b9e827c69d98fb0c1241925961ebb5184752a25a` (the foundation with its 2026-09-23
amendment), vendored byte-for-byte beside this file. The disposition table is at lines
126-136 of that copy; F-PROV's five acts are at lines 116-124.

Every name in backticks in the right-hand column is a row of the acceptance ladder in
`docs/ck-bus.md` (r2). A foundation row no ladder row claims is a named
exclusion, and the cell says who keeps it. `crates/ck-bus/tests/grant_generation.rs`
checks that every foundation row below appears here and that every cell names a ladder
row or says "Named exclusion".

The foundation's `ckcred-*` skip names do not carry into this mapping: the amendment
removed the vault ops they waited on (root keys by ceremony, per-process keys in
ck-bus's memory, revocation as NATS state). `bus-module-unlanded` leaves a row's skips only under the per-gate rule in
the spec's Intent, and deleting F-PROV is a prefrontal commit carried by
`prefrontal-seat-unnamed`.

## Disposition table rows

| Foundation row | ck-bus acceptance disposition |
| --- | --- |
| A1 supervised server lifecycle | `A1 supervised server lifecycle` and `A1 argv non-injection` |
| A2 seed absence / child signing | Seed absence for the ck-bus process and for a participant: `User JWT and seed absence`. Child signing under the amendment: the child holds no seed and signs its connect nonce through ck-bus, claimed by `Credential delivery and attestation` (attested principal, `ckbus_principal_direct` refusal) and `Vault authorization` (the `reserved:ckbus` grant, identity present versus absent). The foundation's observation that a child inherits its parent's launch-nonce principal stays an observation (spec Non-goals); no row gates it. |
| A3 census and revocation | `A3 census write and issuance recovery`, `A3 revocation`, `Spawn-stream consumer`, and `Spawn reconciliation and census recovery`. The foundation's fourth revocation step (vault delete) is gone; its control is now the in-memory key drop inside `A3 revocation` (`ckbus_credential_superseded`). |
| A4 stream durability, fanout and re-key | Named exclusion: fanout and cursor durability stay in the prefrontal and commons rigs, re-run against the built module by `A6/A7 prefrontal re-run`; CALLO's device re-key is not ck-bus's. The provisioning F-PROV does for A4 is claimed by `Install bootstrap and own users` (streams, census bucket), `Credential delivery and attestation` (participant durables, bound credentials) and `A3 census write and issuance recovery` (census keys). The membership re-mint is `Membership lifecycle`. |
| A5 trust and link | `Federation account and isolation`, `Leaf configuration`, `Leaf link, labelled`, `Seal outbound`, `Open inbound`, `Federation sequence and crash`, `Split store-and-forward`, and `Peer removal fence` |
| A6 trait conformance | Named exclusion: the commons and prefrontal trait conformance rig keeps it; its re-run with the module present is `A6/A7 prefrontal re-run`. |
| A7 no regression | Named exclusion: the prefrontal delivery-path rig keeps it; its re-run with the module present is `A6/A7 prefrontal re-run`. |
| A8 sentinel probe | `Module health answer` and `A8 sentinel probe` |
| A9 grant conformance on a live server | `Grant generation` (the generator, the golden diff rule and the partial-token, deny and out-of-lexicon controls), `A9 grant conformance` (the live-server half; the golden-commit half records `prefrontal-seat-unnamed`), `Install bootstrap and own users` (the bus-module and system-account users and account JWTs the rig minted itself), `User JWT and seed absence` (user JWTs ck-bus builds), `Dead-letter` (`c_ckbus_dead`), and `Membership lifecycle` (the membership sub-arm) |

## F-PROV acts

| F-PROV act | ck-bus acceptance disposition |
| --- | --- |
| 1. Creates the five streams and the census bucket | `Install bootstrap and own users` and `Machine id and account` |
| 2. Creates every durable workload consumer | `Credential delivery and attestation` (issuance step 4 creates the participant's `c_{agent_id}` durables) and `Dead-letter` (`c_ckbus_dead`) |
| 3. Mints per-process participant credentials | `Credential delivery and attestation`, `User JWT and seed absence`, `Signer wire shape`, and `Vault authorization` |
| 4. Writes one census key per credential | `A3 census write and issuance recovery` |
| 5. Mints the bus-module credential and executes the membership re-mint | `Install bootstrap and own users`, `A8 sentinel probe`, and `Membership lifecycle` |
