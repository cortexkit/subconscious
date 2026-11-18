# ck-nats-engine

Version 0.1.3 starts every leaf link with TLS, before any NATS `INFO` is sent.
Both ends must support TLS-first. The hub's TLS 1.3 listener asks `ck-nats-server`
to sign handshakes with Ed25519 over the engine's stdin/stdout channel.
The hub waits for its first listener state before opening the server, and a
listener port or certificate change requires a restart. Version 0.1.1 added
`ck-nats-engine --version`, which prints the build version
without reading configuration or opening the store.

The private child of `ck-nats-server`. It reads the existing NATS configuration
and owns the embedded server. Standard input and output are exclusively the v1
newline-delimited JSON channel; server logs go to standard error. Closing stdin
shuts down the server and exits within five seconds.

## Build and test

Install Go 1.26.4 and `patch`, then run from the repository root:

```sh
bash scripts/build-nats-engine.sh
GOOS=linux GOARCH=amd64 bash scripts/build-nats-engine.sh
GOOS=linux GOARCH=arm64 bash scripts/build-nats-engine.sh
GOOS=darwin GOARCH=arm64 bash scripts/build-nats-engine.sh
GOOS=windows GOARCH=amd64 bash scripts/build-nats-engine.sh
GOOS=windows GOARCH=arm64 bash scripts/build-nats-engine.sh
bash scripts/build-nats-engine.sh --test -v -run 'Test(Codec|Signer|Remote|Engine)'
```

Core's `x64` architecture is Go's `amd64`. The default output is
`target/nats-engine/GOOS-GOARCH/ck-nats-engine` (`.exe` on Windows); an optional
first argument overrides the output path. Run the binary with `-c server.conf`.
Cross-builds are static (`CGO_ENABLED=0`); signing and distribution are separate.

The script downloads the committed module graph, verifies its checksums, copies
the pinned upstream module into a temporary build directory, applies the single
`reload-signaturecb.patch` with zero fuzz, and builds through a temporary module
replacement. It never modifies the module cache or commits upstream source.
Tests must use the same script so they exercise the carried patch.
Release hosts without the pinned Go version can run
`bash scripts/go-toolchain.sh bash scripts/build-nats-engine.sh` (or append
`--test` and the test filter). The helper downloads the official toolchain into
`target/`, verifies a pinned SHA-256, and changes `PATH` only for that command.

The patch only excludes `SignatureCB` from remote reload equality: Go callbacks
cannot be compared with `reflect.DeepEqual`, even when they are unchanged.
Remote names are mapped to synthetic `Nkey` identities (`ck-bus:<name>`), never
to credentials files. The callback takes precedence over upstream seed handling.
Changing the account, deny lists or pin requires a new name; renaming removes the
old remote and adds the new one. Peer pins authenticate the leaf SPKI directly;
hosted pins trust only the presented pinned CA and require a valid chain and DNS
name.

With `--hub`, the engine waits for `state.hub_listener` before starting NATS.
It does not read `server.conf` while idle: ck-bus writes that file during hub
setup. A missing or invalid file after a listener arrives produces `apply_failed`
and leaves the engine waiting for another state.
The leaf listener uses only TLS 1.3 and the supplied public Ed25519 certificate;
each handshake signs through `tls_sign` on the private channel, with a five-second
budget. Later states can retain the listener but cannot replace its port or
certificate. Without `--hub`, a listener state is refused.
