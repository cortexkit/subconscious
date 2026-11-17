# Changelog

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
