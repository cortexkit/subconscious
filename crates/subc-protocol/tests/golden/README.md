# Rust/TypeScript wire-shape golden fixtures

These JSON files are canonical Rust serializations for the subc protocol wire
shapes. The TypeScript client consumes the same fixtures as conformance vectors;
if a Rust shape changes without an intentional fixture update, the golden test
fails and flags TS/Rust drift.

Regenerate after an intentional wire-contract change with:

```sh
UPDATE_GOLDEN=1 cargo test -p subc-protocol --test golden_json
```
