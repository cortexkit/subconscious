# Agent assertion token v1 corpus provenance

## Producer

- Crate: `crates/agent-token-vectors`
- Producer commit: `cba602ae35b0e99d0785b2122c6cc5c39048eae4`
- Corpus: `agent_token_vectors_v1.json`

The producer commit contains the generator, reference verifier, and the emitted corpus bytes. This record is kept beside the corpus rather than inside it because a generated artifact cannot contain the hash of the commit that first contains those bytes.

## Claim-shape authority

The frozen authority is the `prefrontal` repository's
`.cortexkit/alfonso/plans/agent-assertion-token-v1.md` claim-shape pin. The
corpus was minted against claim-shape version 1 from that file; the pin wins if
it ever disagrees with a generated vector.

## Regeneration

From the `subconscious` repository root, run:

```sh
cargo run -p agent-token-vectors --bin generate
```

Then run `cargo test -p agent-token-vectors`; it checks both that the checked-in
corpus matches generator output and that two generator invocations are
byte-identical.

## Versioning rule

A claim-shape change is a `v` bump and requires a **new corpus file**. Never
edit `agent_token_vectors_v1.json` in place to represent a different claim
shape; consumers vendor the named version and its producer commit.
