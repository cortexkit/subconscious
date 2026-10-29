# app.open v1 samples

These JSON bodies are produced by the daemon's real channel-0 `app.open`
dispatcher and handler. Each file pairs a request with its response body or
error body, exactly as framed by the daemon. Frame headers are not included.

The samples cover `launched`, `already_running`, and every refusal in check
order: `app_open_not_used` (macOS), `app_open_disabled`, `app_unknown`,
`app_not_installed`, and `app_verification_failed`. The already-running check
comes between the unknown-app and installation checks.

The test injects a platform and policy, and replaces the OS launcher with a
fixed outcome. Successful samples use fixture pid `4242`; no machine paths or
real process ids are recorded. The verification-failure sample uses a fixed
`app_verification_failed` error, not a machine-specific signature diagnostic. These samples
describe the wire format, not which apps the production policy admits.

Regenerate from the repository root (then inspect the diff):

```sh
UPDATE_GOLDEN=1 cargo test -p subc-daemon --lib app_open_vectors_match_real_handler_bytes --locked
```

Without `UPDATE_GOLDEN`, the same test byte-compares every file and
`SHA256SUMS`. The checksum file covers the seven JSON samples, in filename
order. To check it independently, run `sha256sum --check SHA256SUMS` in this
directory. Git preserves the bytes on every platform with `-text` attributes.

