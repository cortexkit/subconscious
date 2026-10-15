# Changelog

## Unreleased

- `AskRequest` decodes the optional `headAgentId` (spelled with a lowercase d, exactly as the producer writes it): the lead agent of a worker's ask, resolved by the producer from the verified scope chain. A head's own ask has no such key and decodes as `nil`. It is for placing the ask under its lead agent and confers no approval authority. No error cases change.
- Add `ScopeBindProofCodes.required` and `.mismatch`, the daemon's two refusal codes for a scoped route open that is missing its scope's bind proof or carries the wrong one. Callers receive them as `FedFailure.moduleError` with a readable message, the same case other terminal refusals use, so no public SubcFed enum gains a case and existing exhaustive switches keep compiling. Both codes are terminal (`crates/subc-protocol/tests/golden/decision_tables.json`): never retry the open.

- `BoardLaneItem` decodes four optional fields: `etaMs`, `changedAtMs`, `followUp` and `noteRef`. Each accepts the snake_case wire spelling or the camelCase spelling after key normalization, like the other lane types. Items without them decode as before.
- Expose module-declared bus events on `CatalogEntry`, preserving the manifest declaration shape returned by `catalog.list`.
- Add golden-reply regressions for `scope.describe` so run deadlines (`scope.expires_at_ms`) and future stamp fields stay readable through `callManagement`, while legacy replies without a deadline remain supported. The client already returns reply dictionaries without restricting their keys.
- The new `call_mode` field identifies classic, codemode or flow calls on provider-side `route.bind` messages. This package opens routes as a client rather than accepting provider binds, so there is no Swift bind decoder to update.
