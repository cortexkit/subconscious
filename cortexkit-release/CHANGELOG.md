# Changelog

## 0.1.2

- Use `cortexkit-lease` for durable approval and lease-holder replacements.

## 0.1.1

- Refuse non-public phases without an execution implementation instead of
  reporting successful gates without evidence.
- Reconcile attempted effects across confirmed declaration rebinds without
  losing their original intent or permitting a duplicate executor call.
- Allow observational `verify_readback` phases after publication while retaining
  the pre-publication-only CI gate ordering rule.
- Print callable train names in abandonment/rebind recovery instructions and
  status actions rather than journal basenames.
- Preserve immutable local-command output across retries, re-execution, and
  interrupted attempts by atomically reserving monotonically numbered logs.
- Add explicit per-phase publication artifact selection and refuse overlapping
  targets before provider access; keep the single-phase default of all artifacts.
