# CortexKit credentials contract — MOVED

The authoritative contract lives in the claustrum repository:

    claustrum/docs/cortexkit-credentials-contract.md

This file was a mirror and it drifted (caught 88 lines stale by a consumer's
mason deriving a fix from it, 2026-08-29). Mirrored contract docs are the
same defect as a digest printed instead of written: a fact that does not sit
next to the thing it describes goes stale silently and reads as
authoritative. Read the claustrum copy; do not restore content here.

Error-class vocabulary changes are announced by CKCRED the same way wire
field changes are (CONSUMER-IMPACT), per the bump-announcement contract
requested by synapse's vault client (conservative-fallback consumers need
the announcement to keep unknown-class handling optimal).
