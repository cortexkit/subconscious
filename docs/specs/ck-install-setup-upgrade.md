# ck install / setup / upgrade — alpha distribution for subc + AFT (+MC)

Status: DRAFT r4 — alpha amendment, controlling before implementation.

## Amendment record and controlling authority

This amendment is the controlling normative authority for the alpha distribution
campaign. It folds the final chair rulings from the 2026-08-29
`ck-install-setup-upgrade` campaign, refire 5. If this document conflicts with
an earlier campaign draft, an implementation plan, or the superseded r1 text,
this amendment wins. An implementation conflict must be surfaced; code must
not silently choose a different contract.

The following r1 clauses are superseded in their entirety. This list is
exhaustive: all r1 clauses not named here remain normative only when they do
not conflict with this amendment.

| Superseded r1 clause | Controlling replacement in this amendment |
| --- | --- |
| **Flow**, “Test matrix before any user” | [Alpha support and release inventory](#alpha-support-and-release-inventory) fixes the supported target set and adds the release-inventory gate. |
| **ck setup**, step 1, “Detect platform + existing state” | [Refusal classes](#refusal-classes), [Setup command model](#setup-command-model), and [MC detection](#mc-detection) define the supported-target, dry-run, and detection contracts. |
| **ck setup**, step 2, “Fetch + verify companion binaries … (daemon, ck-subc-mcp, ck-aft; MC optional)” | [Release assets and component availability](#release-assets-and-component-availability) fixes the managed binary set, makes MC wiring-only for alpha, and defines release-incomplete refusal. |
| **ck setup**, step 4, “Windows service (sc.exe / windows-service crate path)” | [Per-user runtime registration](#per-user-runtime-registration) requires the non-elevated `\\CortexKit\\subc-daemon` scheduled task instead of an elevated Windows service. |
| **ck setup**, step 5, “detect standalone AFT (NDJSON transport) and standalone MC; OFFER conversion” | [AFT conversion](#aft-conversion) disables automatic AFT detection for alpha, and [MC detection](#mc-detection) supplies MC’s owner-pinned read-only two-tier detector and offer rules. |
| **ck setup**, step 7, “service dereg, PATH unlink, binaries removed” | [Installation destinations, PATH, and ownership](#installation-destinations-path-and-ownership) and [Setup command model](#setup-command-model) limit uninstall to the ownership inventory and retain user data. |
| **Settled by operator**, item 2, “the standalone-conversion offer is the same code path as a later add” | Accepted conversion still uses the normal component-add path, but [AFT conversion](#aft-conversion) permits AFT conversion only through the explicit confirmed verb during alpha. |
| **ck upgrade**, “Check” bullet, including comparison against releases for “subc / aft / mc” | [Update checks](#update-checks) and [Upgrade ordering and target restarts](#upgrade-ordering-and-target-restarts) exclude MC from alpha upgrades and define the interactive check deadlines. |
| **ck upgrade**, “Upgrade” bullet, including a single `supervisor.restart` path | [Upgrade ordering and target restarts](#upgrade-ordering-and-target-restarts) separates module, daemon, and self-update restart behavior. |
| **ck upgrade**, “Ordering” bullet | [Upgrade ordering and target restarts](#upgrade-ordering-and-target-restarts) retains modules before daemon and makes `ck` self-update the final target. |
| **ck upgrade**, “Channels” bullet | [Alpha support and release inventory](#alpha-support-and-release-inventory) and [Release lanes](#release-lanes) define the supported inventory gate and AFT’s cross-repository release dependency. |

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

The fixed alpha support set is exactly:

- `darwin-arm64`
- `linux-x64`
- `windows-x64`

No other OS/architecture tuple is an alpha target. WSL is treated as Linux and
requires a working systemd user session; without it, setup gives a typed
refusal rather than attempting a Windows runtime path.

The release inventory gate runs before the operator-assisted matrix. For each
supported tuple, it verifies the actual archives and matching `.sha256`
sidecars required by the selected components. A supported target with a missing
required asset is **release-incomplete**, not unsupported. The pre-user matrix
covers macOS, Windows 11 in Parallels, Ubuntu LTS, and Fedora, subject to the
fixed architecture set above.

## Release index (controlling, 2026-09-03)

This section supersedes every clause below that resolves a component's release
by querying the GitHub REST API. `ck setup`, `ck upgrade`, the update check,
and the bootstrap installers resolve releases from **one signed document that
CortexKit owns**, never from `api.github.com`. Operator ruling, 2026-09-03,
after the first macOS operator drive: three findings were GitHub's release
model disagreeing with ours (tag prefixes, train ids, sibling-crate versions
that appear nowhere in a release) and a fourth was the API's unauthenticated
rate limit, shared per source address. The question the installers ask —
*what is the current release of each component, and what are its verified
assets* — is ours to answer.

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
and no GitHub fallback. Key custody: the private key is held as opaque
record `cortexkit:release-index-signing:1:ed25519-pem` in the claustrum
vault and deployed to the index worker as a secret; rotation ships a new
`ck` with the new key under generation `:2` before the worker switches.

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
`/install`) owns the index. It ingests on the org `release` webhook
(`published`, `edited`, `deleted` — HMAC-verified with the webhook secret) and
on a daily cron, rebuilding the whole document from every component's
releases using a server-side GitHub token (5,000 req/h; the per-user
budget disappears from the design). Per component it resolves the current
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

The alpha binary inventory is `ck`, `ck-subc`, and `ck-subc-mcp` from the
subconscious release lane, plus `ck-aft` from the AFT release lane. MC has no
alpha release archive: alpha may wire an already available MC component, but
it must not download an MC archive and MC is excluded from `ck upgrade`.

## Installation destinations, PATH, and ownership

The owner-fixed managed binary destinations are:

| Platform | Binary home | PATH mechanism |
| --- | --- | --- |
| macOS and Linux | `~/.local/share/cortexkit/bin` | Append a marker-delimited `# cortexkit-managed PATH begin` / `# cortexkit-managed PATH end` block to the appropriate user shell profile: `~/.zshrc`, `~/.bashrc`, or Fish `config.fish`. |
| Windows | `%LOCALAPPDATA%\cortexkit\bin` | Update the user-scope `PATH` through the `HKCU` `Environment` registry value. |

No installation path or PATH operation requires elevation. The installer
checks whether an existing destination binary matches its archive sidecar
digest. On a match it reports the existing binary and skips placement; PATH and
ownership-record steps remain idempotent.

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
- `ck setup --with aft,mc` selects optional components non-interactively.
- `ck setup --dry-run` is strictly read-only: it prints the full setup plan,
  including observations, proposed mutations, and refusals, without creating,
  replacing, registering, starting, or changing anything. Its plan must be
  equivalent to the plan used by the corresponding non-dry-run invocation.
- `ck setup aft --convert` and `ck setup mc --convert` are explicit conversion
  paths and require confirmation before any mutation.
- `ck setup --uninstall` prints a removal plan, deregisters only manifest-owned
  runtime registrations, removes manifest-owned links and binaries, and leaves
  user configuration and stores on disk with a retention note.

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
malformed-or-locked classification, and false-positive fixtures. This campaign
must not invent that contract.

## MC detection

MC automatic detection is read-only and selects one data directory in this
order:

1. `MAGIC_CONTEXT_TEST_DATA_DIR`
2. `MAGIC_CONTEXT_STORAGE_DIR`
3. `$XDG_DATA_HOME/cortexkit/magic-context/`
4. `~/.local/share/cortexkit/magic-context/` when `XDG_DATA_HOME` is unset

Each override names the directory containing `context.db`. On Windows the
literal default is `%USERPROFILE%\.local\share\cortexkit\magic-context\`.
No AppData path exists or is probed for MC detection; this is the user-profile
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

`SQLITE_BUSY` while opening or reading a qualifying MC database means
**installed and live**. It deliberately produces no automatic conversion offer
because the detector cannot safely determine tier-2 durable state while the
installation is live. `ck setup mc --convert` remains available for that
classification and every other classification, and it always requires
confirmation before mutation.

Automatic MC detection on Windows has an additional owner gate: it may offer
conversion only after the MC owner confirms that standalone Windows installs
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
release data, and produces an ordered plan. MC is never an upgrade target in
alpha because it has no alpha archive.

For every selected non-self target, the executor derives the matching archive
and sidecar, verifies SHA-256, creates a rollback copy, places the candidate,
warm-executes the destination inode, and post-verifies PID, destination inode,
health, and version. Failed post-verification offers rollback; accepting it
restores the prior inode. The command prints evidence for each completed stage.

Restart semantics are target-specific:

- Managed modules restart through `supervisor.restart`, with initiation
  acknowledgement and completion polling; their clients absorb the transition
  through retry patience.
- The daemon restarts through its platform service manager, with a 30-second
  drain and service-manager initiation acknowledgement. It does not use the
  module restart path.
- `ck` self-update has no runtime restart. It is always the final selected
  target, after all modules and the daemon have completed, so its failure
  cannot undo or strand their completed sequence.

On Unix, self-update verifies a temporary replacement and atomically renames it
over the current executable. The running process retains the prior inode, and
a subsequent invocation must execute the new destination inode. On Windows,
`ck.exe` is renamed to `ck.exe.old`, the verified replacement is placed as
`ck.exe` without elevation, and the next successful invocation deletes `.old`.

## Release lanes

The subconscious Windows release lane is in this repository’s alpha scope.
AFT release-lane work is cross-repository and belongs to the AFT owner; no
campaign slice may implement it here. Until that lane publishes the required
supported-target archive and matching sidecar, an AFT selection on an otherwise
supported target fails with the **release-incomplete** refusal.

MC is wiring-only for alpha, ships no alpha release archive, and remains
excluded from `ck upgrade`. It may adopt the archive convention only when its
CLI joins a later release channel.

The macOS release lane uses a Developer ID Application identity, App Store
Connect API-key authentication, one identity for every binary in an archive,
and `notarytool submit --wait`. It hard-fails if only an Apple Development
identity is available. Staple-pending is the documented degradation; ad-hoc
development identities never enter the release path.

## Verification requirements

Tests must prove observable effects and preserved state, not only success
messages. At minimum they cover the three fixed target tuples; unsupported and
release-incomplete refusals; direct archive and sidecar naming; installer
idempotence and its prohibition on executing setup; dry-run read-only behavior
and plan equivalence; all per-user runtime registrations with immediate
liveness; manifest-limited uninstall and self-update ownership; explicit AFT
conversion; MC precedence, two tiers, foreign and torn state, `SQLITE_BUSY`,
and the Windows owner gate; the bare-`ck` 800 ms budget; the 10-second
per-target `--check` bound; target-specific restart behavior; upgrade ordering;
and both self-update replacement lifecycles.

Before user installation, ShellCheck, PSScriptAnalyzer, Rust `clippy -D
warnings`, and the operator-assisted platform matrix are required. The release
inventory gate must succeed before that matrix begins.

## Non-goals

- Automatic AFT detection in alpha.
- Automatic updates, daemon-triggered update checks, OS notifications, and GUI
  installer work.
- An elevated Windows service, MSI, winget, Homebrew formula, or Linux distro
  package in alpha.
- MC alpha release archives or MC participation in `ck upgrade`.
- Cloudflare mirror-route deployment in this campaign.
- New daemon RPCs or module wire-protocol changes.
