# Changelog

## 0.1.11

- Name the schema rule a refused value broke in the new `Refusal::rule` field
  (`RefusalRule`: type, const, enum, length, pattern, numeric bounds, list
  rules, undeclared and required keys, references, credentials, variants and
  inheritance bounds), with the bound or allowed values the schema declares
  and never the refused value. The field is not serialized, so a serialized
  `Refusal` is unchanged. `Refusal::new` leaves it empty; `with_rule` sets it.

## 0.1.10

- Refuse `propertyNames` and `not` on ordinary settings nodes instead of silently ignoring them.
  Opaque JSON schemas still support both keywords.

## 0.1.9

- Reuse compiled schema patterns across validation and property lookup.

## 0.1.8

- Add settings-view source attribution for effective empty objects without changing tuple review.

## 0.1.7

- Expose `child_schema` so callers can use the same property and pattern lookup as validation.

## 0.1.6

- Apply `x-ck-bound` only to workspace and project writes, not to user settings.
  Bounds have no effect on user-only fields, including opaque JSON fields.

## 0.1.5

- Add `Schema::review_tuple_attributed` to identify the layer refused during
  review. Leave the existing `Schema::review_tuple` API and refusal format intact.

## 0.1.4

- Refuse only the old value-reference shapes `{"env": "<string>"}` and `{"file": "<string>"}`.
  An object whose single `env` or `file` key holds a map or list, such as a declared map of
  shell environment variables, is ordinary data.
- An optional object (`x-ck-required: false`) with no default stays absent until a layer sets
  it, instead of being built from its members' defaults and failing its own required members.
  The first layer that sets it starts from those member defaults.

## 0.1.3

- Allow scalar type lists at managed fields and user-only opaque JSON fields.
  Opaque values replace atomically and use their declared JSON constraints.
- Allow leaves marked `x-ck-required: false` to omit a default and remain absent until set.

## 0.1.2

- Let the first settings layer choose a tagged-union variant even when the
  schema supplies a default, while keeping the variant fixed for higher layers.
- Allow optional credential references with no default; omit absent references
  from effective settings and refuse schemas that declare a credential default.

## 0.1.1

- Admit `x-ck-setup` on top-level settings sections, validating unique feature ids,
  root-relative recommended values and credential-free targets without applying
  recommendations as defaults.

## 0.1.0

- First release of shared configuration schemas, layer and bound validation,
  credential redaction, canonical JSON and domain-separated digests. Module
  authors can use these types to declare configuration and review immutable
  changes without handling resolved secrets.
- Provide approval, generation, delivery and rollback records for the approved
  configuration store. Approval channels distinguish verified desktop review,
  interactive OS confirmation, phone consent and unapproved development
  placements; a recorded proposal is not an approval.
- The configuration gate ships with the stamp off. Unstamped installs behave as
  before; applications must explicitly enable stamping before treating the
  approved store as configuration authority.
