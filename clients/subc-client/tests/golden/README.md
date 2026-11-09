# Scope request byte fixtures

The `scope-*.json` files were generated from the repository's Rust
`subc_protocol::session::ModuleControlRequestFromModule` type, using a temporary
Cargo harness under `target/scratch/`. No Rust source in the repository was
changed. Each input was decoded into that enum and written with
`serde_json::to_vec(&request)`, not serialized through `serde_json::Value`.
Unlike the parsed-JSON goldens in `crates/subc-protocol/tests/golden`, these
files retain serde's declaration order and have no trailing newline.

`provider-scopes.test.ts` supplies the same full record and minimal record to
the provider API and compares the actual outbound UTF-8 body against these
files, byte for byte. It also checks that explicit Rust defaults serialize to
the same minimal bytes. The full record includes all serializable fields;
its attribution combinations exercise encoding, not daemon admission.

To regenerate when the Rust wire types change, use a scratch Cargo package
depending on the checkout's `subc-protocol` and `serde_json`. For each fixture,
read its JSON into `ModuleControlRequestFromModule`, then write it back with
`serde_json::to_vec`. Change example values in the TypeScript test and fixture
input together when adding coverage; never derive the expected bytes from the
TypeScript encoder being tested.

Federated claim fixtures include `agent_name` or `flow_name` on each named claim
kind. Their identity fields are unchanged; the names were added to the harness
inputs and serialized through Rust again. Claimless fixtures remain unchanged.
