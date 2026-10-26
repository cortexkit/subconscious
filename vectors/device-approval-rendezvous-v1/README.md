# Published rendezvous device-approval vectors v1.1

`rendezvous.json` is a byte-for-byte copy of the rendezvous client's published
vector set, version `v1.1`. Its SHA-256 is
`931da5aefc104993d21d66f674caf1b41fa62c0c97c6f7a49bf783c942e2ddf0`.

The set covers locally computed approval fingerprints/comparison codes and
rendezvous-computer enrollment, possession and removal transcripts, including
signature refusal cases. Swift tests reproduce the fingerprint/code cases and
the common A, P and D suffix bytes and check Ed25519 signatures. All private
seeds in this file are public test-only values, never production keys.

Swift tests cover all three fingerprint/code cases, including the accepted
ordinary X25519 key with bit 255 set. They account for all 39 refusal rows and
assert rejection for all 38 applicable rows. `computer-actor-key` is the sole
non-applicable row: an Ed25519 computer actor requires a literal dash, whereas
the row's canonical ULID has the required key-id shape for a P-256 phone actor.

Run `shasum -a 256 -c SHA256SUMS` to check the pinned bytes. This vendored set
has no local generator; phone-owned vectors are in `../device-approval-phone-v1`.
