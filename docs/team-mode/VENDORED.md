# Vendored corpora: where these bytes came from

These files are **copies**. They are minted by `cortexkit-account` and vendored
here so this repo's conformance work has fixed bytes to check against.

Recording the source commit is the one provenance check a consumer *can* run. A
producer can regenerate an artifact from its generator and require byte-identity;
a consumer holds only the artifact, so the strongest available assertion is
"these bytes are the ones published at a named commit". Without that, a hand-edit
that keeps every module id live passes every check in `scripts/fleet/check-vendored-module-ids.sh` —
that script asserts the ids are *live*, never that the bytes are *authentic*.

Before this file existed, the source commit lived only in the peer message that
requested the re-vendor. That message scrolls away; the corpora do not.

## Current vendoring

Source: `cortexkit-account` commit `52188788fafc0ab4684282d5d88e58e9a234ffe7`

| file | sha256 (first 16) |
| --- | --- |
| `fixtures/room2-ckcred-fixtures.json` | `951428d63b8f2dff` |
| `appendices/room2-ckcred-fixtures.json` | `951428d63b8f2dff` |
| `conformance/vectors/ckcred/room1-contract-samples.json` | `39e6b2d4e78e1c8f` |
| `fixtures/room1-contract-samples.mirror.json` | `39e6b2d4e78e1c8f` |

The two identical pairs are deliberate — each corpus is vendored to two
locations, and they must move together. A digest mismatch *within* a pair means
one copy was re-vendored and the other was not, which is worse than both being
stale: two of our own mirrors disagreeing reads as a contract dispute, and the
first place anyone looks is the contract rather than the copy.

## Re-vendoring

1. Re-vendor **every** file in the table, not only the ones a producer names.
   A producer can only notify about copies it knows exist; enumerating our own
   copies is the half that finds all of them. The room1 pair was found this way
   after a notice that named only the room2 pair.
2. Update the commit and the digests above in the same change.
3. Run `scripts/fleet/check-vendored-module-ids.sh` and confirm it reports a
   non-zero **recognised** count. A zero-unknown result alone cannot distinguish
   a clean corpus from a scan that silently read nothing.

Re-minting changes every signature, so a re-vendor is a deliberate amendment at
a named commit rather than a silent refresh.
