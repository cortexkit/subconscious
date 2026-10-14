# Changelog

## Unreleased

- Add `ScopeBindProofCodes.required` and `.mismatch` string constants and readable mappings through the existing terminal `FedFailure.moduleError` path. No public SubcFed enum gains a case; existing exhaustive app switches remain source-compatible. Both codes are tested against `crates/subc-protocol/tests/golden/decision_tables.json`, which classifies them as terminal: clients must not retry a refused open.

- `BoardLaneItem` decodes four optional fields: `etaMs`, `changedAtMs`, `followUp` and `noteRef`. Each accepts the snake_case wire spelling or the camelCase spelling after key normalization, like the other lane types. Items without them decode as before.
- Expose module-declared bus events on `CatalogEntry`, preserving the manifest declaration shape returned by `catalog.list`.
- Add golden-reply regressions for `scope.describe` so run deadlines (`scope.expires_at_ms`) and future stamp fields stay readable through `callManagement`, while legacy replies without a deadline remain supported. The client already returns reply dictionaries without restricting their keys.
- The new `call_mode` field identifies classic, codemode or flow calls on provider-side `route.bind` messages. This package opens routes as a client rather than accepting provider binds, so there is no Swift bind decoder to update.
