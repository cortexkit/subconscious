# Isolated configuration acceptance

Run on Linux. The SDK scenarios and sample runner spawn only children with an
isolated `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, and an
explicit connection file. Native
keychain access is not used: the development daemon needs both `test-support`
and `SUBC_TEST_CONFIG_GATE=1`, and its executable name must start with `ckdev-`.
The switch selects a file key under the explicit store root and a scripted
substitute for the OS interactive confirmation. It bypasses polkit availability
only in the guarded test process. Creating `fail-anchor` in that root denies anchor persistence;
removing it lets the ordinary background retry recover. No switch reader or
policy override is compiled into a default-feature daemon.

Build prerequisites serially from the workspace root, retaining the ordinary
target directory:

```sh
mkdir -p target/scratch/config-sdk
cargo build --locked -p subc-core --bins
cp target/debug/ck-subc target/scratch/config-sdk/default-daemon
cp target/debug/ck target/scratch/config-sdk/ck
cargo build --locked -p subc-core --bin ck-subc \
  --features subc-core/test-support,subc-daemon/test-support,subc-daemon/dev-desktop-approval
cp target/debug/ck-subc target/scratch/config-sdk/dev-daemon
cargo build --locked --target-dir target \
  --manifest-path crates/subc-core/tests/config_status_sdk/doc-sample/Cargo.toml
export CONFIG_SDK_DEFAULT_DAEMON="$PWD/target/scratch/config-sdk/default-daemon"
export CONFIG_SDK_DEV_DAEMON="$PWD/target/scratch/config-sdk/dev-daemon"
export CONFIG_SDK_CK="$PWD/target/scratch/config-sdk/ck"
export CONFIG_SDK_RUST_SAMPLE="$PWD/target/debug/config-sdk-doc-sample"
export CONFIG_SDK_BUN="$(command -v bun)"
cargo test --locked -p subc-core --features test-support \
  --test config_status_sdk config_status_sdk -- --include-ignored
```

The Rust sample fixture extracts the visible Rustdoc example at build time and
delegates directly to those hooks. Its logging wrapper is harness code, not part
of the documented hooks needed by an adopting module. The whole visible example
must fit within 60 lines. The TypeScript test launches the published fixture itself.
The transparent TCP tap forwards authentication and module frames unchanged,
retaining only config traffic. A declaration can be delayed after HELLO to test a
real recovery refusal; responses and refusal codes are never synthesized.
