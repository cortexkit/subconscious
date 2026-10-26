# Phone device-approval vectors v1

`phone.json` freezes exact UTF-8 transcripts, SHA-256 and signatures for phone
registration (both proofs), approve/cancel, removal/rotation and merge veto.
All scalars and seeds in this directory are **public test-only keys**. They are
not production credentials and do not demonstrate Secure Enclave use.

P-256 signatures use RFC 6979 deterministic ECDSA/SHA-256 and P1363 low-S.
Ed25519 signs raw UTF-8 bytes. The protocol's standalone P-256 verifier vector
is included verbatim, with both low-S and high-S encodings. Rejection lists
exercise grammar, closed challenge objects, transcript comparison and signature
encoding/domain boundaries. `routes.json` pins issuance and signed decision body
bytes plus success/refusal replies for all ten phone flows. `reads.json` covers
the four GET status/list envelopes, including numeric-cursor refusals. Swift tests
consume every case. Collect's HTTP snapshot uses a published test-only account
key and the existing canonical-payload rendezvous signature format.

Key refusals include every one of the fourteen X25519 low-order encodings
(libsodium's seven `has_small_order` rows and each row with bit 255 set), for
fingerprint derivation (F), phone enrollment actions (A) and phone removals (D).
F refusal rows produce no fingerprint or code. Weak and non-point Ed25519 keys
are refused for machine and rendezvous targets. The X25519 blocklist
is exactly the public `crypto_scalarmult/curve25519/ref10/x25519_ref10.c` list;
it is not an expanded contributory-DH or full point-order check. Swift tests pin
the constants byte for byte and reject all fourteen Ed25519 weak/noncanonical
encodings from [libsodium 1.0.18's `ge25519_has_small_order` blocklist](https://github.com/jedisct1/libsodium/blob/1.0.18/src/libsodium/crypto_core/ed25519/ref10/ed25519_ref10.c#L1021-L1075)
as well.

Install `requirements.txt` in a virtual environment, then run:

```sh
python3 generate.py         # regenerate
python3 generate.py --check # byte-for-byte currency check
shasum -a 256 -c SHA256SUMS
```

The generator checks all three JSON files and `SHA256SUMS`; it uses no clocks, random
keys, network calls or host-dependent JSON formatting.
