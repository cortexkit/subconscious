# Changelog

## Unreleased

- Decode the optional registry-row `capabilities` list only when it contains at most eight unique, byte-sorted tokens matching `^[a-z][a-z0-9-]{0,31}$`; expose the list on `RdvRegistryRow`. `servesFed` is true when the list contains `fed-serve`, meaning the computer says it accepts direct connections from paired devices. It is a routing hint only: it does not authenticate a peer or grant access, and a device still connects only to computers it has paired with.

- Add test-only coverage verifying that two live clients for different peers can share one SQLite store without mixing peer state, and that concurrent store instances assign unique, durable sequence numbers to every mutation.

- `BoardLaneWait` decodes the optional `lastIdleTransitionMs`, accepting `lastIdleTransitionMs` or `last_idle_transition_ms`: the board owner's most recent idle transition, repeated on each wait and absent when the owner has no turn anchor. A held board uses it to age a wait's `rotten` verdict while the server answers `unchanged`. It is encoded only when present, and the memberwise initializer gains a defaulted parameter, so existing call sites compile unchanged.
- `AskRequest` decodes the optional `headAgentId` (spelled with a lowercase d, exactly as the producer writes it): the lead agent of a worker's ask, resolved by the producer from the verified scope chain. A head's own ask has no such key and decodes as `nil`. It is for placing the ask under its lead agent and confers no approval authority. No error cases change.
- Add `ScopeBindProofCodes.required` and `.mismatch`, the daemon's two refusal codes for a scoped route open that is missing its scope's bind proof or carries the wrong one. Callers receive them as `FedFailure.moduleError` with a readable message, the same case other terminal refusals use, so no public SubcFed enum gains a case and existing exhaustive switches keep compiling. Both codes are terminal (`crates/subc-protocol/tests/golden/decision_tables.json`): never retry the open.

- `BoardLaneItem` decodes four optional fields: `etaMs`, `changedAtMs`, `followUp` and `noteRef`. Each accepts the snake_case wire spelling or the camelCase spelling after key normalization, like the other lane types. Items without them decode as before.
- Expose module-declared bus events on `CatalogEntry`, preserving the manifest declaration shape returned by `catalog.list`.
- Add golden-reply regressions for `scope.describe` so run deadlines (`scope.expires_at_ms`) and future stamp fields stay readable through `callManagement`, while legacy replies without a deadline remain supported. The client already returns reply dictionaries without restricting their keys.
- The new `call_mode` field identifies classic, codemode or flow calls on provider-side `route.bind` messages. This package opens routes as a client rather than accepting provider binds, so there is no Swift bind decoder to update.
