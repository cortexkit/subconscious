# Pairing QR watch events

These are callosum's locally generated `callosum.pairing_qr_watch` notice
vectors. They are not vendored from another repository, so no entry in
`scan-vendored-ids.py`'s vendored digest table is needed; its general scan still
covers this directory.

`events.json` contains ten cases. Decode each `event_json` string and take its
UTF-8 bytes as the exact StreamData body, with no trailing newline. JSON key
order is part of this vector contract: `state` first, then `source_addr` for
`foreign_attempt`; for `claimed`, `device_name`, `source_addr`, `fingerprint`.
The outer JSON document's formatting is not part of the event body.

## Reader rules

- The known states are `open`, `foreign_attempt`, `claimed`, `granted`,
  `declined`, and `expired`. Readers must skip unknown `state` values.
- A stream ends after exactly one `granted`, `declined`, or `expired` notice,
  followed by StreamEnd. Cancellation closes without a terminal notice; it
  does not decline the pairing window.
- Notices are the current state followed by observed changes, not a replay log.
  Rapid store transitions can be coalesced. A `claimed` notice means pending
  confirmation, not merely an in-progress handshake.
- `claimed.fingerprint` is always the full 32-character lowercase hex key
  fingerprint. `device_name` is the account registry name after `display_safe`,
  or JSON `null` when no name is available. An empty name remains empty.
  No device name or address is invented.
- `source_addr` is an IP address without a port, for both `claimed` and
  `foreign_attempt`. IPv6 has no socket brackets. Each foreign IP is reported
  at most once per subscription.
- A pending claim missing its peer key or a valid source IP is a store
  inconsistency: callosum logs an ERROR and ends that stream with `expired`,
  not a partial `claimed` notice. Pairing confirmation words, word hashes, and
  PSKs never appear in watch notices.

## Generation and verification

The cases are generated through the production watch projection and typed
serializer. `pairing_qr::tests::watch_events::vectors_match_production_builder`
compares the file byte for byte and pins its case count. Regenerate on Linux:

```sh
cargo test --locked -p fed-module --lib print_pairing_qr_watch_vectors -- --ignored --nocapture
```

Copy each `@@@ file <name>` block, excluding the marker lines, into the named
file. The printer generates `events.json` and `SHA256SUMS`; its digests cover
the current README and the generated event bytes.

`pairing_qr::tests::watch_events::vector_checksums` checks every digest and
requires the manifest to name every file other than itself. The deterministic
claimed vectors use public key bytes `07` repeated 32 times; they contain no
real account or device data.
