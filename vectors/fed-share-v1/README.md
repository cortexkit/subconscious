# share-v1 test vectors

These vectors pin the wire bytes of share sessions (fed-wire §3.7, §6, §8.10):
the share hello, the three share frames, the five new bye codes, the share-call
error codes, and the phone's disposition table. They are normative for bytes,
and a phone library should run every row.

Callosum writes them; nothing here is copied from another repository. They are
generated from fed-core's production builders and decoders by
`crates/fed-core/src/session_share_tests.rs`, never by hand.

## Files

- `hellos.json`: share hello shapes. Each case is a hello `header` (and its
  whole frame as `frame_hex`) received by the side named in `receiver` (its
  Noise role). `expect: "accepted"` rows must be accepted and read as the
  named `session_kind`; `expect: "refused"` rows must be answered with the
  named `bye` and closed. Covered: the share hello, an ordinary hello without
  `session_kind`, a computer hello listing `share-v1`, a share hello carrying
  `connection_attempt_id`, six unknown or malformed `session_kind` values, and
  a share hello sent by the Noise responder.
- `frames.json`: every share frame, valid and malformed. `frame_hex` is one
  whole fed-frame; a `declaration_only` row stops after the body length,
  because the receiver must refuse it before buffering any body byte. Decode
  each row with its `max_body_bytes`. `expect: "valid"` rows decode to
  `decoded` (with `never_ran` written out as `false` when the header omits it);
  `expect: "violation"` rows must close the session with
  `bye fed_protocol_violation`. `violation` is fed-core's own label for the
  refusal and is informative only. `on_ordinary_session` is what an ordinary
  session must answer when it receives the frame.
- `codes.json`: `byes` holds the five new bye codes, each as a whole bye frame
  with its sender and meaning. `errors` holds the codes a `share_result` error
  can carry, new and existing, each as a frame without `never_ran` and, where
  `never_ran_allowed`, a second frame with `never_ran: true`.
- `dispositions.json`: the disposition table. `table_row` numbers the rows of
  the table in fed-wire §3.7. Each row is an answer (a `share_result` frame, a
  close after the mark, or a close before `share_ready` with or without an
  earlier mark), whether the attempt's claim is still the newest, and the
  `outcome` fed-core's `share_disposition` computes for it after decoding the
  frame.
- `SHA256SUMS`: the SHA-256 of every other file here, this README included.

## Checks and regeneration

`session::tests::share::share_v1_vectors_match_the_production_builders`
rebuilds all four JSON files and fails if any committed byte differs, then
checks the row counts. `share_v1_vector_checksums` checks `SHA256SUMS`
against the files. To regenerate after a deliberate change, run

```
cargo test -p fed-core --lib print_share_v1_vectors -- --ignored --nocapture
```

write each `@@@ file <name>` block it prints to that file, refresh
`SHA256SUMS` with `sha256sum README.md codes.json dispositions.json
frames.json hellos.json > SHA256SUMS`, and commit them together.

Module ids in these files are checked by `test-vectors/scan-vendored-ids.py`.
`frames.json` alone may carry the three invalid ids its malformed-module cases
need.
