# Setup and upgrade contract

The rules the bootstrap installers, `ck setup`, `ck upgrade` and uninstall
follow. The task-first guide is [`install-and-upgrade.md`](install-and-upgrade.md);
the implementation is `crates/subc-core/src/setup/` and `scripts/install/`.
Where this document and the code disagree, the code is authoritative and the
disagreement is a defect to report.

## Flow

```text
curl -fsSL https://cortexkit.io/install | bash
irm https://cortexkit.io/install/win | iex
ck setup
ck upgrade
```

The repository-canonical shell and PowerShell installers bootstrap only `ck`.
They are mirrored at `cortexkit.io`; the mirror must not become a fork. The
installers derive, fetch, verify, and place `ck`, then print the exact next
`ck setup` command. They never execute setup, write configuration, register a
runtime, or detect standalone components. All lifecycle management after
bootstrap belongs to Rust verbs in `ck`.

`ck setup` and `ck upgrade` are idempotent, check-then-act commands. A refused
operation prints its evidence and exits non-zero. Required checks must set the
exit status directly; decorative shell gates are forbidden. Partial setup is
resumable, and already-correct state is reported without mutation.

## Refusal classes

The installer, setup planner, and upgrade planner must distinguish these
non-zero refusals:

- **unsupported-platform:** The host target is not one of the fixed alpha
  target tuples. This refusal is made before looking for release assets and
  names the unsupported tuple.
- **release-incomplete:** The host target is supported, but a required
  convention-derived archive or its matching digest sidecar is unavailable
  from the release lane. This refusal names the missing asset or sidecar and
  the component whose release lane is incomplete.

A digest mismatch, extraction failure, placement failure, failed warm
execution, failed health verification, and incompatible user configuration
are separate evidence-bearing refusals; none may be reported as either
platform or release availability.

## Alpha support and release inventory

The fixed alpha support set is exactly (`AlphaTarget::ALL` in
`crates/subc-core/src/setup/model.rs`):

- `darwin-arm64`
- `linux-x64`
- `linux-arm64`
- `windows-x64`
- `windows-arm64`

No other OS/architecture tuple is an alpha target. WSL is treated as Linux and
requires a working systemd user session; without it, setup gives a typed
refusal rather than attempting a Windows runtime path.

The release inventory gate runs before the operator-assisted matrix. For each
supported tuple, it verifies the actual archives and matching `.sha256`
sidecars required by the selected components. A supported target with a missing
required asset is **release-incomplete**, not unsupported.

## Release index

`ck setup`, `ck upgrade`, the update check,
and the bootstrap installers resolve releases from **one signed document that
CortexKit owns**, never from `api.github.com`. GitHub's release model does not
match CortexKit's (tag prefixes, release trains, crate versions that appear in
no release), and its unauthenticated rate limit is shared per source address.
The question the installers ask, *what is the current release of each
component, and what are its verified assets*, is answered by CortexKit.

### Document

```text
https://cortexkit.io/releases/v1/index.json       the index (application/json)
  response header  X-CortexKit-Signature-Ed25519: <base64>   Ed25519 over the exact response body
https://cortexkit.io/releases/v1/index.json.sig   the same signature as a detached file, for tooling and humans
```

The signature is over the exact bytes served as `index.json`. **`ck` reads
the signature from the response header of the one request that fetched the
index** — a body and a signature that arrived together are consistent by
construction, whereas two URLs (or two KV keys) have independent cache and
propagation clocks and can be read torn for up to a minute after every
release, which would refuse a correct index as tampered. The producer
stores body and signature as ONE KV value and serves both views from it;
`.sig` exists for tooling, `ck` never fetches it. `ck` embeds the verifying
public key (`RELEASE_INDEX_PUBKEY`, 32 bytes) and **refuses** an index whose
signature header is absent or does not verify — there is no unsigned mode
and no GitHub fallback. The private key is never in this repository; it is deployed to the index
worker as a secret. A key rotation ships a new `ck` carrying the new public
key before the worker switches to signing with it.

```jsonc
{
  "schema": 1,
  "channel": "alpha",                       // one document per channel; alpha is the only channel today
  "generated_at_ms": 1788425000000,
  "components": {
    "core": {
      "repository": "cortexkit/subconscious",
      "release": "subc-core-v0.14.1",       // the source release's tag, verbatim
      "published_at_ms": 1788400000000,
      "version": "0.14.1",                  // present when the tag carries one
      "train": null,                         // present for train-tagged components (mc): the tag's build sha id
      "requires_core": null,                 // modules: the oldest core (ck-subc) this release registers against; null = no declared floor
      "assets": {
        "darwin-arm64": {
          "ck-subc": { "url": "https://github.com/cortexkit/subconscious/releases/download/subc-core-v0.14.1/ck-subc-darwin-arm64.zip",
                       "sha256": "<64 hex, VERIFIED at ingest against the downloaded bytes>", "bytes": 6149696,
                       "reports": "0.14.1" },   // substring `<binary> --version` must print; null when the release does not say
          "ck-subc-mcp": { "url": "…", "sha256": "…", "bytes": 4100000, "reports": null }
        },
        "linux-x64": { … }, "linux-arm64": { … }, "windows-x64": { … }
      }
    },
    "aft": { … }, "mc": { … }, "insula": { … }, "claustrum": { … }, "synapse": { … }
  }
}
```

Rules:

- **Assets are listed, not chosen.** The index lists every `{name}-{os}-{arch}.zip`
  the release carries. Which binaries a component needs on a target is `ck`'s
  table (`component_binaries_for_target`); the index never restates it.
- **A component absent from `components` has no published release** (the
  owner has not shipped one, or the latest is a draft). A component present
  but missing an asset for the host target is the `not yet published`
  outcome for that target. A component whose release the ingester
  **refused** (see below) is absent, with the refusal visible only to the
  owner — a user never sees a half-ingested release.
- **`reports`** is the placement acceptance: after placing a binary, `ck`
  runs `<binary> --version` and requires the `reports` substring when it is
  non-null; when null the binary must execute and print a name and a
  version (`RunsAndReports`), and provenance rests on the verified sha256.
  The ingester derives `reports` from the tag (`v<ver>` and
  `<crate>-v<ver>` → the version for the tag-named binary only; a train tag
  → the full build sha for every binary) and prefers an owner-published
  `release-manifest.json` asset when present (`{"binaries": {"<name>": {"reports": "…"}}}`).
- **`requires_core`** is a module release's declared daemon floor: the
  oldest `ck-subc` version whose HELLO parser accepts this release's
  manifest, as a plain `MAJOR.MINOR.PATCH`. The owner publishes it in
  `release-manifest.json` (`{"requires_core": "0.17.20", "binaries": {…}}`)
  and the ingester copies it verbatim; absent means null, and null means
  no declared floor (today's behaviour). The ingester refuses a release
  whose `requires_core` is present but not `MAJOR.MINOR.PATCH`, and
  ignores the key on `core` itself. Why it exists: a module built against
  subc-protocol ≥ 0.19 sends a HELLO that ck-subc < 0.17.20 refuses
  ("missing field `consumes`"), so placing such a module beside an old
  daemon fails at registration — a fact no release stated until now.
- **Digests are verified at ingest.** The ingester downloads every listed
  archive once, computes sha256, and compares it to the sidecar; a mismatch
  refuses the whole release. `ck` still verifies the sidecar-equal digest
  from the index against the bytes it downloads — the index is the source
  of the expected digest, the sidecar files remain on the release for
  humans and other tooling.
- **Freshness.** `ck` treats an index older than 7 days
  (`generated_at_ms`) as `index_stale` and refuses to install from it;
  the update check renders it as `updates: index stale`. The worker
  regenerates on every release event and on a daily cron, so a stale index
  means the ingester is down, never that nothing was released.
- **Typed refusals** in `ck`: `index_unreachable` (network/5xx),
  `index_signature_invalid`, `index_malformed`, `index_stale`. None of them
  install anything; all name the URL and, for signature failures, the key
  generation `ck` expected.

### Producer: the index worker

The worker at `infra/cortexkit-io-worker` (the same worker that serves
`/install`) owns the index. It HMAC-verifies and acknowledges accepted org
`release` webhooks (`published`, `edited`, `deleted`) immediately; one Durable
Object coordinator rebuilds the whole document once per burst after a
30-second debounce, single-flight, including requests from the daily cron,
using a server-side GitHub token, so no user's request budget is spent. Per component it resolves the current
release by the owner's rule — `latest` non-draft for every component except
mc, whose current release is the newest by `created_at` among
`ck-mc-*`-prefixed prereleases (owner-ruled; the repo's Latest surface
belongs to its npm product) — downloads each listed archive and sidecar,
verifies, derives `reports`, canonicalizes (stable key order, no
whitespace), signs, and writes `index.json` + `index.json.sig` to KV in one
put. A release that fails verification is **refused**: the previous good
entry for that component stays, the refusal is logged with the asset and
the reason, and the owner is notified (a GitHub issue on the component's
repository, opened once per offending tag). The serving path is
`GET /releases/v1/index.json[.sig]` with `Cache-Control: public, max-age=60`;
an admin `POST /releases/v1/reingest` (bearer) exists for bootstrap and
operator repair.

Bytes stay on GitHub release assets for alpha (CDN, sidecars, no new
storage); mirroring archives to R2 is a later, independent step.

### Bootstrap installers

`install.sh` / `install.ps1` read `index.json` for `ck`'s asset url and
sha256 and verify the downloaded archive against that sha256. They do not
verify the index signature (no Ed25519 in a bootstrap shell); bootstrap
trust is TLS to `cortexkit.io`, and the placed `ck` verifies the signed
index on its first `ck setup`. This is stated, not hidden.

## Release assets and component availability

Release assets use the direct convention:

```text
{binary}-{os}-{arch}.zip
{binary}-{os}-{arch}.zip.sha256
```

`os` is `darwin`, `linux`, or `windows`; `arch` is `arm64` or `x64`. The
matching per-asset sidecar is the only digest source. Installers and upgrade do
not use a platform mapping table or an aggregate digest manifest.

The binaries each component needs on each target are listed in
`component_binaries_for_target` (`crates/subc-core/src/setup/components.rs`).
Core is `ck-subc` and `ck-subc-mcp` from this repository's release lane; `ck`
itself is placed by the bootstrap installer. Magic Context has no alpha
release archive on Windows: setup may wire an already available Magic Context
component there, but must not download one.

## Installation destinations, PATH, and ownership

Fresh installer destinations separate the operator command from managed daemon and module binaries:

| Platform | `ck` command destination | Managed binary home | PATH mechanism |
| --- | --- | --- | --- |
| macOS and Linux | `<data home>/cortexkit/cmd/ck` | `<data home>/cortexkit/bin` | Append a marker-delimited `# cortexkit-managed PATH begin` / `# cortexkit-managed PATH end` block to the appropriate user shell profile (`~/.zshrc`, `~/.bashrc`, or Fish `config.fish`), with only `cmd/` on PATH. |
| Windows | `%LOCALAPPDATA%\cortexkit\cmd\ck.exe` | `%LOCALAPPDATA%\cortexkit\bin` | Add only `%LOCALAPPDATA%\cortexkit\cmd` to the user-scope `PATH` through the `HKCU` `Environment` registry value. |

Existing installations are not moved by `ck setup` or `ck upgrade`; their owners move their PATH entry by hand if they want only `ck` on PATH. No automatic migration is attempted.

No installation path or PATH operation requires elevation. The installer checks whether an existing destination binary matches its archive sidecar digest. On a match it reports the existing binary and skips placement; PATH and ownership-record steps remain idempotent.

`installer-manifest.json` in the platform data directory is the ownership
inventory. It records every mutation managed by installation, setup, uninstall,
or self-update, including binary placements, PATH changes, and runtime
registration. Setup, uninstall, and self-update may mutate or remove only
items established in this inventory; retained user configuration and component
stores are never treated as managed binaries.

## Setup command model

The command surface is:

- `ck setup` installs and wires core, then offers eligible optional components.
- `ck setup aft` and `ck setup mc` explicitly add a component.
- `ck setup --with aft,mc` selects optional components explicitly.
- `ck setup --dry-run` is strictly read-only: it prints the full setup plan,
  including observations, proposed mutations, and refusals, without creating,
  replacing, registering, starting, or changing anything. Its plan must be
  equivalent to the plan used by the corresponding non-dry-run invocation.
- When stdin and stdout are both terminals, a mutating setup (including
  uninstall) prints the same plan and proposed diffs as `--dry-run`, then asks
  `Continue? [y/N]`. Only `y` or `yes` proceeds. Any other answer, including
  end-of-input, exits successfully with `Nothing was changed.` No-op plans and
  refusals do not ask. `--yes` (`-y`) skips this question, not safety checks or
  the separate `--confirm` required for conversion. If either stream is not a
  terminal, setup proceeds without a question for scripted callers. The shell
  and PowerShell bootstrap installers only print `Next: ck setup`; they do not
  invoke it.
- `ck setup aft --convert` and `ck setup mc --convert` are explicit conversion
  paths and require confirmation before any mutation.
- `ck setup --uninstall` prints a removal plan, deregisters only manifest-owned
  runtime registrations, removes manifest-owned links and binaries, and leaves
  user configuration and stores on disk with a retention note.

Before planning installation, setup inspects existing unowned binary
destinations and reads `--version` with a two-second deadline. A newer binary,
or a newer installed daemon even when inventory-owned and otherwise correct,
refuses the entire plan: `ck setup will not downgrade it. Nothing was changed.`
The refusal names the observed version (including a reported build suffix) and
the latest release version. An older or unreadable unowned binary refuses with
its path, explains that setup did not install it, and asks the operator to move
it aside and run `ck setup` again. Neither refusal downloads binary archives,
writes configuration, nor registers services, in a real run or a dry run.
An unowned binary reporting the same version remains eligible for adoption:
apply downloads the release and adopts only byte-identical files; a version
match alone does not establish ownership.

The installed core release comes from the managed `ck-subc --version`, whether
the daemon is running or stopped. The connection file's `daemon_ver` identifies
the separately versioned `subc-daemon` library and must not be compared with a
core release or a module's `requires_core` floor.

Upgrade discovery uses the managed binary's core release as well. After
activation, reading the disk version alone is not evidence about the running
daemon. `ck-subc --build-info` is an offline JSON probe (core version, build SHA,
lock digest), evaluated before runtime/configuration or daemon startup. Upgrade
requires its core version and `--version` to equal the expected release, then
compares its build SHA and lock digest with the authenticated running daemon's
`server.describe` response (`ck --json daemon`). A healthy old daemon with a
different build does not satisfy readiness or verification. Missing, malformed,
or dirty offline build identity refuses verification; absent or mismatching
running identity never satisfies readiness. No new wire field is required.

Configuration writes are additive. Setup prints the proposed diff before a
write and refuses a conflicting user-owned value by naming the key and leaving
the file byte-identical. Adding one component must not disturb another
component’s valid configuration.

After successful wiring, setup validates with `ck daemon triage`, `ck health`,
and `ck fleet lint`, and then prints the MCP harness snippet. The relevant
triage command and selected-component health checks must succeed.

## AFT conversion

Automatic AFT standalone detection is disabled for alpha. Alpha must not infer
an AFT installation from `aft.jsonc`, NDJSON transport evidence, an environment
variable, a process, or any other heuristic, and it must not present an
automatic AFT conversion offer.

The only alpha AFT conversion entry point is `ck setup aft --convert`. It
prints the conversion plan, requires confirmation, and then uses the normal
component-add path. Declining it changes binaries, configuration, runtime
registration, and stores by zero bytes.

An implementation-grade automatic AFT detector is an AFT-owner, post-alpha
dependency. Before a later detection slice may be implemented, that owner must
specify its path, environment precedence, qualifying predicate,
malformed-or-locked classification, and false-positive fixtures. This installer implementation
must not invent that contract.

<a id="mc-detection"></a>
## Magic Context detection

Magic Context automatic detection is read-only and selects one data directory in this
order:

1. `MAGIC_CONTEXT_TEST_DATA_DIR`
2. `MAGIC_CONTEXT_STORAGE_DIR`
3. `$XDG_DATA_HOME/cortexkit/magic-context/`
4. `~/.local/share/cortexkit/magic-context/` when `XDG_DATA_HOME` is unset

Each override names the directory containing `context.db`. On Windows the
literal default is `%USERPROFILE%\.local\share\cortexkit\magic-context\`.
No AppData path exists or is probed for Magic Context detection; this is the user-profile
resolution of the Unix fallback.

The detector opens `context.db` using a read-only SQLite `mode=ro` URI. It does
not create directories, checkpoint WAL state, write the database, or infer an
installation from a directory or from
`~/.config/cortexkit/magic-context.jsonc`.

- **Tier 1** requires a real, valid SQLite `context.db` containing a
  `schema_migrations` table with at least one row whose `version` is below
  `10000`.
- **Tier 2** requires tier 1 plus a positive total row count across
  `compartments`, `memories`, and `tags`.

Only tier 2 is eligible for an automatic conversion offer. Tier-1-empty state
is fresh and gets no offer. A foreign SQLite database without qualifying
migration evidence and torn state with `context.db-wal` or `context.db-shm`
but no `context.db` get no offer. Sidecar absence is not evidence either way.

`SQLITE_BUSY` while opening or reading a qualifying Magic Context database means
**installed and live**. It deliberately produces no automatic conversion offer
because the detector cannot safely determine tier-2 durable state while the
installation is live. `ck setup mc --convert` remains available for that
classification and every other classification, and it always requires
confirmation before mutation.

Automatic Magic Context detection on Windows has an additional owner gate: it may offer
conversion only after the Magic Context owner confirms that standalone Windows installs
write the literal default path above. Until that confirmation, Windows uses
only the explicit confirmed `ck setup mc --convert` path. A no-override Windows
fixture is required.

## Per-user runtime registration

Runtime registration is persistent and user-scoped on every alpha platform:

| Platform | Runtime identifier and definition | Registration and immediate start |
| --- | --- | --- |
| macOS | launchd label `cortexkit.subc` at `~/Library/LaunchAgents/cortexkit.subc.plist` | Register with launchd, then bootstrap/kickstart it immediately. |
| Linux | systemd user unit `cortexkit-subc.service` under `~/.config/systemd/user/` | Run `systemctl --user enable --now` so the unit is both persistent and live now. |
| Windows | Scheduled task `\CortexKit\subc-daemon` | Create the user-session task at logon, then run it immediately with `schtasks`. |

The Windows path is native PowerShell, requires neither Git Bash nor WSL, and
requires no UAC prompt or elevation. An elevated Windows service is not an
alpha target. Setup must separately report current liveness and persistent
registration; logon registration alone is not sufficient acceptance.

### Service-definition reconciliation

Setup observes definitions even when the daemon is healthy. An outdated owned
definition adds `update the daemon service definition (includes restart after a
crash)` to both the dry run and interactive confirmation. Current definitions
add no update step. An unowned or user-edited definition reports a blocking
refusal before any setup mutation, like an unowned binary; apply checks ownership
again before writing. A registered service without its owned definition file is
also refused. Symlinks and non-regular definition files are not overwritten.

Comparison is semantic, not byte-based:

- macOS: parse the plist dictionary, including all keys, typed booleans/integers,
  strings and ordered arrays. Dictionary order, XML indentation, comments and
  entity spelling do not matter; string contents and array order do.
- Windows: parse the submitted XML and export the registered task with
  `Export-ScheduledTask`. Compare element names/namespaces, attributes, text and
  children, sorting only unordered schema containers. Resolve exported SID
  UserIds to account names. Ignore registration Date/Author and known optional
  schema defaults added by Task Scheduler; non-default settings and extra
  elements remain visible as edits. The registered task must agree with the
  inventory-owned XML before it may be updated.
  Registration comparison additionally accounts for version-1.4 tasks exporting
  `UseUnifiedSchedulingEngine=true` when omitted at creation, and omitting the
  default `RunLevel=LeastPrivilege` and logon-trigger `Enabled=true`. This
  projection is never used to establish file ownership: an explicit engine value,
  elevation, disabled trigger, command-path change or other setting still
  participates in comparison. Both explicit engine values remain distinct.
- Linux: parse section names and directive/value lists, ignoring blank lines,
  full-line comments and whitespace around `=`. Repeated directives retain their
  order. All directives, including `ExecStart`, delegation, stop and restart
  policies, participate. Old units can differ even though this release does not
  change `Restart=on-failure`.

The `runtime-definition` inventory row records canonical `definition_semantics`
after a successful write. Later semantic edits invalidate that ownership. Older
rows have only a path and identifier: accept them only when the file matches a
known historical generated template for this daemon path and Windows account
(including the pre-recovery templates). This cannot distinguish a user edit
that exactly reproduces a historical template. Unknown legacy variants are
conservatively refused, not adopted merely because their path is recorded.

Application is platform-specific:

- A loaded, live macOS agent requires bootout/bootstrap to read a replacement
  plist. The update step says it restarts the daemon **and every module** and
  runs only through the accepted setup plan. Ordinary `ensure` still refuses a
  changed live definition without writing or issuing commands; a daemon that
  becomes live after a non-restarting plan requires replanning. Configuration
  restart requirements coalesce into the same planned reload.
- Windows updates the existing registration in place with `Set-ScheduledTask`
  (generated action, logon trigger, principal and settings), never `/End` or
  `/Run` for a live setup update. Microsoft's [Set-ScheduledTask documentation](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/set-scheduledtask)
  states that changes to the definition do not affect its running instance;
  [New-ScheduledTaskSettingsSet](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset)
  supplies settings usable for an existing registration. New registrations
  still use `schtasks /Create /XML` and immediate start. The update merges only
  the generated settings into the existing settings object, preserving other
  scheduler options such as `UseUnifiedSchedulingEngine`; it reads setup's UTF-8 XML
  explicitly as UTF-8.
- Linux writes the owned unit and runs `daemon-reload`; an already live service
  is not stopped or restarted by setup. Starting an inactive service remains
  part of setup. Reloaded restart/stop policies apply through systemd; changed
  process arguments take effect on its next start.

Daemon upgrade activation uses the same ownership checks before stopping and
regenerates inside its existing restart: macOS bootout → write → bootstrap;
Windows `/End` → write/update → `/Run`; Linux write → daemon-reload → restart.
There is exactly one restart, not a setup start followed by another restart.
An unowned definition refuses activation without overwriting it. No daemon
activation (for example a module-only or no-op upgrade) means no definition
refresh; `ck setup` is the explicit reconciliation path in that case.

## Update checks

Only a user-invoked `ck` process may use or refresh the 24-hour update metadata
cache. The daemon never checks for updates or contacts release infrastructure.

Bare `ck` starts cache refresh asynchronously and has a hard 800 ms refresh
budget. On cache expiry, timeout, offline release infrastructure, or rate
limiting, the dashboard completes normally and prints the cached state with its
age, for example `updates: not checked (cache 3d old)`. The refresh may never
block the bare command.

`ck upgrade --check` is the explicit update-check surface. It may wait at most
10 seconds per target. On expiry it fails with a typed failure naming that
target. It prints the current target plan and availability without replacing
binaries or restarting a runtime.

## Upgrade ordering and target restarts

`ck upgrade` discovers installed targets, reads binary `--version` output and
daemon catalog build information, compares them with the applicable latest
release data, and produces an ordered plan. Magic Context (`ck-mc`) is an
upgrade target on the targets that have its archive; its releases are build
trains, so its version is compared as a train id rather than a number.

For every selected non-self target, the executor derives the matching archive
and sidecar, verifies SHA-256, creates a rollback copy, places the candidate,
warm-executes the destination inode, and post-verifies PID, destination inode,
health, and version. Failed post-verification offers rollback; accepting it
restores the prior inode. The command prints evidence for each completed stage.

The ladder runs the daemon first, then the modules, then `ck`. A module built
against a newer wire crate can send a HELLO that an older daemon refuses (the
manifest diet dropped fields a pre-0.17.20 daemon required), so a module
replaced ahead of the daemon would restart into a registration refusal and the
ladder would stop before the daemon that accepts it was touched. The reverse
is safe because the daemon parses older manifests leniently; that premise is
pinned by a test that registers a pre-diet manifest against the current daemon,
so the ordering fails loudly if the premise ever moves.

The floor a module declares (`requires_core`) is enforced by the planner,
never by the executor:

- `ck upgrade` compares each module's `requires_core` with the **core
  version the plan will leave running** — the index's core release when
  core is in the plan, the installed daemon otherwise. A module whose
  floor is above that is not upgraded; the plan carries a `Refusal` naming
  the module, its floor, and the core version that falls short, and every
  other target proceeds. Daemon-first ordering already covers the common
  case (core and the module bump together); the check exists for the case
  the order cannot fix, an index whose core is behind a module's floor.
- `ck setup <module>` compares `requires_core` with the **installed** daemon
  version (`"${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit/bin/ck-subc" --version`; the catalog build when the daemon is live).
  Below the floor it refuses before downloading anything — "requires core
  ≥ X, installed Y; run `ck upgrade` first" — because setup must not
  silently replace the daemon to satisfy a module. A fresh `ck setup`
  installs core from the same index first, so the floor holds by
  construction there.
- Comparison is numeric on `MAJOR.MINOR.PATCH`; a `requires_core` that does
  not parse is a refusal of that module naming the malformed value (the
  ingester should have refused it; `ck` does not guess).

Restart semantics are target-specific:

- The daemon restarts through its platform service manager, with a 30-second
  drain and service-manager initiation acknowledgement. It does not use the
  module restart path.
- Managed modules restart through `supervisor.restart`, with initiation
  acknowledgement and completion polling; their clients absorb the transition
  through retry patience.
- Completion polling waits up to 180 seconds for a restarted target to be live
  and healthy: the drain may spend its full 30 seconds whenever a consumer holds
  a route open, and a module's own startup follows it (`aft` reports healthy
  only after warming its roots, which has taken over a minute on a loaded
  host). A budget equal to the drain alone refused restarts that succeeded
  seconds later.
- `ck` self-update has no runtime restart. It is always the final selected
  target, after the daemon and all modules have completed, so its failure
  cannot undo or strand their completed sequence.

On Unix, self-update verifies a temporary replacement and atomically renames it
over the current executable. The running process retains the prior inode, and
a subsequent invocation must execute the new destination inode. On Windows,
`ck.exe` is renamed to `ck.exe.old`, the verified replacement is placed as
`ck.exe` without elevation, and the next successful invocation deletes `.old`.

## Release lanes

The subconscious Windows release lane is in this repository’s alpha scope.
AFT release-lane work is cross-repository and belongs to the AFT owner; no
installer change may implement it here. Until that lane publishes the required
supported-target archive and matching sidecar, an AFT selection on an otherwise
supported target fails with the **release-incomplete** refusal.

Magic Context publishes `ck-mc` archives for macOS and Linux as build-train
prereleases. It has no Windows archive, so on Windows it is wiring-only and
excluded from `ck upgrade`.

The macOS release lane uses a Developer ID Application identity, App Store
Connect API-key authentication, one identity for every binary in an archive,
and `notarytool submit --wait`. It hard-fails if only an Apple Development
identity is available. Staple-pending is the documented degradation; ad-hoc
development identities never enter the release path.

## Verification requirements

Tests must prove observable effects and preserved state, not only success
messages. At minimum they cover the fixed target tuples; unsupported and
release-incomplete refusals; direct archive and sidecar naming; installer
idempotence and its prohibition on executing setup; dry-run read-only behavior
and plan equivalence; all per-user runtime registrations with immediate
liveness; manifest-limited uninstall and self-update ownership; explicit AFT
conversion; Magic Context precedence, two tiers, foreign and torn state, `SQLITE_BUSY`,
and the Windows owner gate; the bare-`ck` 800 ms budget; the 10-second
per-target `--check` bound; target-specific restart behavior; upgrade ordering;
and both self-update replacement lifecycles.

Before a release, ShellCheck, PSScriptAnalyzer and Rust `clippy -D warnings`
must pass, and the release inventory gate
(`scripts/checks/ck-alpha-release-inventory.sh`) must succeed.

## Non-goals

- Automatic AFT detection in alpha.
- Automatic updates, daemon-triggered update checks, OS notifications, and GUI
  installer work.
- An elevated Windows service, MSI, winget, Homebrew formula, or Linux distro
  package in alpha.
- Magic Context release archives or `ck upgrade` participation on Windows.
- Cloudflare mirror-route deployment in this installer implementation.
- New daemon RPCs or module wire-protocol changes.
