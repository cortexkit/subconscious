# Script module through a real daemon

`script_module_real_daemon` runs on macOS, Linux and Windows (Git Bash). It needs
Bun 1.4.4, Node, Rust and the platform's native build prerequisites. It is ignored
by default; a missing Bun, runner or SDK build is a failure when explicitly run.
From the workspace root:

```sh
(cd clients/subc-client && bun install --frozen-lockfile && bun run build)
(cd clients/script-runner && bun install --frozen-lockfile && bun run build:native && bun run build)
cargo test --locked -p subc-core \
  --features test-support,subc-daemon/dev-desktop-approval \
  --test script_module script_module_real_daemon -- --exact --ignored --nocapture
```

On macOS, prefix the commands with `nice -n 10` (including commands inside the
subshells). The runner is compiled with the same build script used for release;
this test does not qualify Developer ID signing or notarization.

The test copies the built daemon and compiled runner into a private install
directory, with the runner beside the daemon as in an installation. It copies
the SDK package's `package.json`, `src` and `dist` (not its native addons) and
bundles `module.ts` using `--conditions=ck-script-runner`. The runner's `--check`
must accept that bundle. The daemon child has isolated home, XDG and Windows
profile directories and an explicit config-store root. Its PATH is empty, so it
cannot find Bun as an interpreter. No environment variables are changed on the
test process.

The first launch is unstamped. The module must reach running, advertise `echo`
in the catalog, and answer a `route.open` request with its supervised PID and the
installed runner's executable path. The existing `test-support` configuration
seam and `dev-desktop-approval` path then stamp the isolated store using a
scripted operator confirmation. A second launch must serve under that stamped
approval. After stopping it, the test replaces the bundle with code that writes
a marker if executed. Enabling the module must return a `config_gated` error naming `program_refused`
and the unapproved program identity, show `script.needs_review`, leave no live
process or catalog entry, keep its successful-spawn count unchanged, and write
nothing.
The unstamped gate intentionally applies content as found; content approval is
only enforced after stamping. On Unix a second isolated daemon also proves that
an otherwise valid script reached through a symlinked folder is refused before
any runner spawn.

Unix script and install paths are resolved to physical paths so macOS's `/tmp`
symlink does not turn the positive case into a path refusal. Windows uses ordinary
`C:\...` drive paths. A separate probe found that the daemon's component-wise
script opener rejects `fs::canonicalize`'s `\\?\C:\...` verbatim form: it opens
`\\?\C:` before appending the root separator, receiving Access Denied (5).
By contrast, executable-byte capture for a compiled module opens the complete
file path and checks only its final component for symlinks, rather than opening
each parent. The probe confirmed that open accepts the complete verbatim path.
The script-path limitation is not fixed or hidden by an expected-panic test here.
