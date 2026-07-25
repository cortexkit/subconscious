# Rust/TypeScript wire-shape golden fixtures

These JSON files are canonical Rust serializations for the subc protocol wire
shapes. Two suites read them, and they catch different things:

- `crates/subc-protocol/tests/golden_json.rs` fails when a Rust shape changes
  without an intentional fixture update.
- `clients/subc-client/tests/golden-conformance.test.ts` fails when a fixture
  changes in a way the TypeScript client cannot speak. This one is what makes
  the fixtures a cross-language contract rather than a Rust-only snapshot: TS
  interfaces erase at runtime, so without code that reads these bytes, a field
  the client declares and a field the daemon sends can disagree with nothing to
  notice until a live handshake.

Regenerate after an intentional wire-contract change with:

```sh
UPDATE_GOLDEN=1 cargo test -p subc-protocol --test golden_json
```
