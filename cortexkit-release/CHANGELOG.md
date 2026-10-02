# Changelog

## 0.1.1

- Refuse non-public phases without an execution implementation instead of
  reporting successful gates without evidence.
- Reconcile attempted effects across confirmed declaration rebinds without
  losing their original intent or permitting a duplicate executor call.
- Allow observational `verify_readback` phases after publication while retaining
  the pre-publication-only CI gate ordering rule.
