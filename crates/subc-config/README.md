# subc-config

The shared, published configuration-gate contract. This crate performs no I/O, vault resolution,
network discovery, approval or persistence. The daemon checks the **approved** schema before asking
an admitted module to review. Modules own full JSON Schema draft 2020-12 validation, merge and
cross-field policy; `Schema::review_tuple` is a module-side helper for the shared merge/bound rules,
not a second authoritative daemon merge engine.

## Canonical JSON and digests

`canonical_json` implements RFC 8785: compact JSON, recursive UTF-16 property sorting, JSON string
escaping, and ECMAScript shortest number formatting. Ryu supplies shortest digits; this crate lays
those digits out using the ECMAScript decimal/scientific notation thresholds. Integers are exact
only within **±(2^53−1)**. Non-integers must be finite f64. Other numbers are refused, never clamped.
The restriction applies even to an integral f64. Null can be canonicalized, but is never a layer
value. No `float_roundtrip` or `arbitrary_precision` serde_json feature is enabled by this crate.

Only this Rust code computes configuration digests. Apps and the TypeScript SDK echo the daemon's
full digest; they do not recompute it. `digest` hashes the canonical bytes of:

```json
{"domain":"subc-config/<purpose>","version":1,"value":"the actual JSON value"}
```

The version is `DIGEST_VERSION`. Purpose constants are `PURPOSE_VALUE`, `PURPOSE_SCHEMA`,
`PURPOSE_DECLARATION`, `PURPOSE_REVIEW`, `PURPOSE_APPLY_PLAN`, `PURPOSE_PLAN`, `PURPOSE_CHANGESET`,
`PURPOSE_GENERATION`, and `PURPOSE_SNAPSHOT`. Unknown purposes are refused. `Digest` validates the
complete `sha256:` plus 64 lowercase hex representation. `raw_digest` is plain SHA-256 over exact
file bytes: comments, BOMs, line endings and absent files remain distinct snapshot inputs.

## Schemas and layers

`vocabulary.schema.json` publishes the policy annotation meta-schema, also exported as
`VOCABULARY_META_SCHEMA`. `Schema::new` normalizes local references and use-site policies. Remote,
recursive, unresolved references, conditional/ambiguous policy compositions and nullable types are
refused. Structural objects require `unevaluatedProperties: false`. Dynamic maps use a constrained
`patternProperties` value schema, not arbitrary additional JSON. Every primitive, replacement
array, item and map value needs a description, a default or `x-ck-required: true`, and effective
`x-ck-apply` and `x-ck-layers`. Structural policies propagate; children may narrow layers, not widen
them. Apply transitions must agree or increase disruption (live to a deferred transition, or a
transition to daemon restart). Process-wide restart fields are user-only.

The apply kinds are `live`, `module_restart`, `daemon_restart`, `next_supervision`, `route_rebind`.
Allowed layers are user, workspace, project, in that order; defaults sit below user. There is no
agent layer. Canonical project/workspace root identity, registry mappings, shared-root deduplication
and completeness of the affected project set are snapshot responsibilities of the caller, not path
lookups performed by this crate. Re-review **every affected approved project tuple** when a lower
layer changes, even when the changeset does not edit that project's file.

Layer overlays inherit absent keys and source attribution. Objects/maps deep-merge; `{}` does not
clear them. Arrays replace in order, including `[]`, with complete items and no id-based merge.
Declared item ids must be unique. Null, unknown keys, type changes and inherited union-discriminator
changes are refused. Discriminated `oneOf` branches must have a unique required const discriminator.
Full effective schema and cross-field checks remain mandatory in the module's review reply; the
shared helper's type/enum/range/required checks do not replace a full draft 2020-12 validator.

`raise_only`/`lower_only` compare finite numbers against the effective lower value; `subset_only`
compares sets of canonical scalars while preserving array order. Boolean and enum monotonic bounds
require `x-ck-ordering`: an enum's values from lowest to highest, or exactly `[false,true]` for a
boolean. No lexical order is inferred. Bounds refuse instead of clamping. Default 5, user 10,
project 15 is valid under raise_only; changing user to 20 refuses project 15 with `bound_violated`
and the project id, until the project override is removed or corrected.

Static enum pickers and closed `x-ck-options` sets reject out-of-set values with `value_invalid`.
Closed dynamic sets are caller-supplied pinned local evidence in `ValidationContext::closed_options`;
missing evidence refuses validation. The daemon remains responsible for dispatching options/actions only through operations declared
read-only in the module manifest pinned to that module's admitted program build, dependency/cache
invalidation, and approval of any side effect. Schema
labels and descriptions are never trusted approval text.

## Credentials and safe records

A credential field has `x-ck-kind: credential_ref` and holds **`{"vault_ref":"<id>"}`**. The id is
opaque, 1–256 bytes of printable ASCII without spaces. This crate checks shape only; a module
resolves the reference through the vault using the daemon's attestation of the caller's module
identity and approved layer/project context. The vault must check that context against the
credential's entitlement rather than trusting a caller-supplied project string. The reference never contains a
secret. Environment/file expansion tokens are refused, never expanded.

Project/workspace references require an entitlement supplied by the validator for the particular
module and project (`ValidationContext::project_credentials`, canonical reference digests). Copying
a user reference into a project does not grant entitlement. Copied user references and an endpoint
override paired with an inherited user credential give `credential_shared`; project-owned
references still require entitlement. Modules declare credential/destination relationships through
`credential_destinations`, not a heuristic based on field names. The caller must populate these
relationships and entitlement evidence when reviewing affected credential fields. Actual vault
entitlement enforcement is a prerequisite to adopting those fields; this crate does not claim to
provide it or support explicit sharing.

Call `Schema::record` **before retaining any submitted value**, including rejected literals. It
retains only the canonical value digest and a `redacted: true` marker at credential paths. Valid
vault-reference ids are redacted too. Unknown keys are redacted conservatively. Refusals contain
only a code, field pointer and project id, never values or schema text. Raw proposal bytes and the
raw `LayerValue` request are ephemeral validation inputs, not history/activity/plan records. Build
`RecordedValue`, `FieldChange` and `Changeset` from this projection; putting raw JSON directly in a
record defeats redaction. The crate does not log submitted values.

## Plans and generations

`Plan` binds base revision, base generation, active-generation vector, daemon incarnation,
`InputVector` and `Changeset`. Input keys identify the complete read set: subc.jsonc, and all layers,
declarations and covered program files for each touched module. Caller snapshot records (and their
digests) must include root/file identities, registry mapping, raw and parsed digests and explicit
absence. `InputState::Absent` is not an empty file. `Plan::check_fresh` compares the base revision
and the saved read set; extra unrelated inputs do not stale a plan. Delivery/proposal/status writes
do not change the base revision. A `plan_stale` refusal carries no replacement digest.

Changesets retain effective old/new values, source changes even for equal values, policy changes,
affected projects, launch manifests and the complete apply plan. These are review records, not
approval authority. `Generation` records serial ids, approved plan/input/declaration digests,
per-module delivery state, active vectors, never-redeliver markers and development provenance.
Generation allocation, commit locking, approval consumption and delivery state transitions belong
to the daemon. Development generations do not move active vectors.

All public structs are non-exhaustive and have constructors; all public enums are non-exhaustive.
Serde names are snake_case. Request structs reject unknown fields; replies and records tolerate
future fields. Constructors and validation helpers do not authorize an apply request.
