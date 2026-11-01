# share-v2-vectors-v1.1

Share-v2 release corpus for `docs/fed-wire.md` §3.7, §5, §8.10 and §10.
Release `share-v2-vectors-v1.1` differs from `share-v2-vectors-v1.0` only in
using invented names, so the vectors can be used in public repositories.
All cases and outcomes are unchanged. The v1.0 bytes remain available at the
`share-v2-vectors-v1.0` tag; all files in `../share-v1/` remain byte-for-byte
unchanged.

| file | coverage |
|---|---|
| `wire.json` | exact hello/header bytes; literal four-row bilateral negotiation expectations; closed framing header refusals; exact/over body declarations; four-frame raw-plus-canonical reservation pool limit |
| `negotiation.json` | eight real hello exchanges across sweep/catalog/phone-feature states; v1 advertisement without a catalog; framed wire responses for mutual v2 and protocol-violation byes for computer-only or phone-only offers |
| `admission.json` | framed canonical body and stored digest; framed SDK-entry dispatch; recorded and canonical-head replay; payload conflict; payload refusals; catalog changes; whole-call non-dispatch; all framing failures |
| `budgets.json` | exact call and declared-byte budgets, including replays/refusals, followed by an over-budget declaration without body bytes |
| `lifecycle.json` | newest-wins with large detached calls, exact doubled per-body charges and remaining server-side pool capacity, and a framed running-row boot sweep |
| `SHA256SUMS` | checksums of this README and all five JSON files |

The comparison tests regenerate all observations in memory and compare exact
file bytes. Fields ending in `_hex` contain exact bytes. Result headers use a
fixed UUIDv7 through the production builder after checking the original reply
id. Large-transfer cases record executable observations rather than storing
many MiB of repeated payload.

Generate on the Linux build server, from the checkout root without `cd`:

```sh
SHARE_V2_GENERATE=1 cargo test --locked -p fed-core -p fed-module --lib share_v2_ -- --nocapture
cargo test --locked -p fed-core -p fed-module --lib share_v2_
```

Producers: `crates/fed-core/src/session_share_tests.rs` and
`crates/fed-module/src/runtime/share_serving_tests.rs`. Generation writes the
JSON files only. Recompute SHA256SUMS from those bytes and update the release
pins in `test-vectors/scan-vendored-ids.py`. After publication, a later
production change must use a new release rather than rewrite a published tag.
