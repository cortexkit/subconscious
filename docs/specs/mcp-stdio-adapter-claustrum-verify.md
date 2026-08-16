# MCP stdio adapter — claustrum read-surface verification

Verdict: **MATCH** (2026-08-16).

Sources checked: `docs/cortexkit-credentials-contract.md` §§4 and 6. No live
claustrum implementation is present in this worktree, so this records the
settled fleet contract rather than probing an unavailable service.

## Adapter resolution call

The adapter's future `{ "handle": "..." }` environment entry resolves through
the runtime route plane using the possession-only operation:

```text
credential.get { handle, min_ttl_ms?, force_refresh? }
```

`handle` is the configured capability handle; no public credential alias or
write operation is used. The adapter will omit the optional refresh controls
unless a later contract amendment requires them.

A successful reply is exactly:

```text
{ payload, expires_at, record_version }
```

`payload` is opaque bytes returned verbatim to the consumer and is the secret
value the adapter will place only in the constructed child environment.
`expires_at` and `record_version` are metadata and are not secret material.

A failed reply is exactly:

```text
{ error: { code } }
```

where the documented codes are `not_found`, `needs_reauth`,
`refresh_unsupported`, `refresh_failed`, `vault_locked`, and `corrupt`.

## Security conclusion

The contract describes reads as route-channel, read-only, trusted-unscoped
possession reads: a caller needs the unguessable handle but no separate read
grant in v1. This matches the adapter spec's assumption. No resolver client is
implemented here; a later child lifecycle implementation may consume this verified contract.
