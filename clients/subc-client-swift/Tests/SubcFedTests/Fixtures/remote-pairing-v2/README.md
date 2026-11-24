# Remote pairing vectors v2 (fed-wire §3.10)

These vectors extend [v1](../remote-pairing-v1/) with the target daemon's
optional `machine_id` on a granted `remote_pair_result`. The grant format is
the same on ceremony and status sessions. All other frames, derivations and
state-machine cases remain as in v1.

`cases.json` gives the exact JSON headers and complete decrypted fed-frames
for grants with and without the id, plus malformed frames the reader rejects.
The keys and ids are fixed test bytes, not real device identifiers.

`fed-core` compares these bytes with its codec in
`remote_pair::tests::grant_machine_id_vectors_match_codec`. To generate a new,
unpublished set, run that test with `REMOTE_PAIR_GRANT_VECTORS_OUT` set to an
output path. Leave published sets unchanged.
