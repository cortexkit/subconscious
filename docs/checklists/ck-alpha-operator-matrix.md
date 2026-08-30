# ck alpha release inventory and operator matrix

Status: **required before any user installation is approved.** This checklist is
an operator record, not a plan to be completed later. Mark a box only after
recording the command output, release URL, CI run, screenshot, or retained file
that proves it. A ruled degradation is acceptable only when it is explicitly
recorded in the release sign-off below.

The fixed alpha targets are `darwin-arm64`, `linux-x64`, and `windows-x64`.
The matrix therefore uses Apple Silicon macOS, x64 Windows 11 in Parallels, and
x64 Ubuntu and Fedora machines. WSL follows the Linux path only when its user
systemd session works; it does not substitute for the Windows machine.

## 1. Release inventory gate — run before a machine is touched

Run this command against the *pinned release-download directories* being
approved, rather than relying on whatever `latest` will point to later:

```bash
bash scripts/checks/ck-alpha-release-inventory.sh \
  --subconscious-release-url "https://github.com/cortexkit/subconscious/releases/download/subc-core-v<version>" \
  --aft-release-url "https://github.com/cortexkit/aft/releases/download/<aft-tag>"
```

The gate downloads and hashes every required archive and matching sidecar:

| Release lane | Required binaries | Required target tuples |
| --- | --- | --- |
| `subconscious` | `ck`, `ck-subc`, `ck-subc-mcp` | `darwin-arm64`, `linux-x64`, `windows-x64` |
| `aft-external-dependency` | `ck-aft` | `darwin-arm64`, `linux-x64`, `windows-x64` |

Each evidence line must name `{binary}-{os}-{arch}.zip`, its exact
`{binary}-{os}-{arch}.zip.sha256` sidecar, and the verified SHA-256. MC is
wiring-only in alpha and has no release archive. The AFT row is intentionally a
separate external dependency owned by the AFT release lane. If that owner has
not published an asset or sidecar, the gate returns `release-incomplete` and
names the missing convention-derived artifact; do not begin the matrix.

| Evidence | Record |
| --- | --- |
| [ ] Gate command and UTC time | |
| [ ] Pinned subconscious tag and URL | |
| [ ] Pinned AFT tag and URL; external owner/contact | |
| [ ] All 9 subconscious archive/sidecar pairs (18 files) passed | |
| [ ] All 3 `ck-aft` archive/sidecar pairs (6 files) passed | |
| [ ] Final `release-inventory: passed; operator matrix may begin` line | |

## 2. CI and release evidence — record before sign-off

| Evidence | Record |
| --- | --- |
| [ ] CI slice URL/commit: installer fetches, verifies, places, prints `Next: ck setup`, and never invokes setup | |
| [ ] CI slice URL/commit: ShellCheck, PSScriptAnalyzer, and Rust `clippy -D warnings` | |
| [ ] CI slice URL/commit: Windows native PowerShell, scheduled-task registration/start, and self-update `.old` lifecycle | |
| [ ] Release naming evidence: all archive and per-asset `.sha256` names follow the direct convention | |
| [ ] Digest evidence: each sidecar is one matching shasum-compatible record; no aggregate manifest was used | |
| [ ] Upgrade evidence: modules before daemon, daemon through its service manager, and `ck` self-update last | |
| [ ] Upgrade evidence: rollback copy, warm destination execution, post-verification, failed-post-verification rollback offer and restore | |
| [ ] Update-cache degradation: offline/rate-limited bare `ck` completes within the 800 ms budget and prints cached `not checked` state with age | |
| [ ] `ck upgrade --check` has a typed per-target failure at the 10 s bound and does not mutate | |
| [ ] macOS Developer ID Application identity and one-identity-per-archive evidence | |
| [ ] macOS notarization `notarytool submit --wait` result | |
| [ ] If unstapled, approved staple-pending degradation and evidence that the archive is notarized | |

## 3. Fresh-machine record — complete once for every required machine

Create one completed copy of this section for each matrix row. Use a new user
profile or VM snapshot with no previous CortexKit placement. Preserve raw logs
and hashes outside the machine before resetting it.

| Machine | Target | Operator | VM/image and date | Log/evidence location | Complete |
| --- | --- | --- | --- | --- | --- |
| macOS (Apple Silicon) | `darwin-arm64` | | | | [ ] |
| Windows 11 in Parallels | `windows-x64` | | | | [ ] |
| Ubuntu LTS x64 | `linux-x64` | | | | [ ] |
| Fedora x64 | `linux-x64` | | | | [ ] |

For **each** row above, record these checks and their evidence:

| Check | Evidence to retain | Pass |
| --- | --- | --- |
| [ ] Run the repository-canonical installer for the target | Exact command, release URL, OS/architecture detection, and exit status | |
| [ ] Installer derives the expected `ck-{os}-{arch}.zip` and matching `.zip.sha256` | Request log and sidecar hash verification output | |
| [ ] Installer places `ck` on the documented user PATH | Binary path, PATH/profile or HKCU mutation, and installer manifest | |
| [ ] Installer did **not** execute setup | Process/log proof plus exact `Next: ck setup` output | |
| [ ] Run `ck setup --with aft` after installation | Full command transcript and selected release identities | |
| [ ] Record current runtime liveness separately from login persistence | Live process/service evidence and launchd/systemd-user/scheduled-task registration evidence | |
| [ ] `ck daemon triage` exits 0 | Command, exit code, and output | |
| [ ] `ck health` and `ck health aft` report selected components healthy | Command output | |
| [ ] `ck fleet lint` passes | Command, exit code, and output | |
| [ ] Setup prints the MCP harness snippet | Output excerpt containing the snippet | |
| [ ] Re-run the installer | No placement change, no setup execution, and `Next: ck setup` repeated | |
| [ ] Re-run `ck setup --with aft` | Explicit no-action-needed output and unchanged managed-state evidence | |
| [ ] Run `ck setup --dry-run --with aft` | Full equivalent plan and proof that it made no mutation | |
| [ ] Conflict refusal | Introduce a conflicting user-owned `subc.jsonc` key; retain non-zero output naming the key and before/after byte hashes | |
| [ ] Add a component | On an AFT-only setup, run `ck setup mc`; retain proof that AFT configuration was not rewritten | |
| [ ] Uninstall retention | Run `ck setup --uninstall`; retain runtime deregistration and managed-binary removal evidence plus the printed retained config/store paths | |
| [ ] Explicit conversion behavior | Record the `ck setup aft --convert` plan, declined no-change proof, accepted confirmation, normal component-add result, and the retained/changed transport explanation; record `ck setup mc --convert` when MC conversion applies | |

## 4. Windows 11 in Parallels — additional mandatory record

These entries apply only to the Windows matrix row and must be completed in
native PowerShell, not Git Bash or WSL.

| Check | Evidence to retain | Pass |
| --- | --- | --- |
| [ ] Native PowerShell installer path | PowerShell transcript; no Git Bash or WSL dependency | |
| [ ] User-session logon registration | `schtasks` evidence for `\CortexKit\subc-daemon` at logon | |
| [ ] Immediate start | `schtasks /Run` evidence and independently observed current daemon liveness | |
| [ ] No elevation | Transcript/screenshot proving no elevation request or UAC prompt | |
| [ ] Self-update replacement | `ck.exe` renamed to `ck.exe.old`, verified replacement placed as `ck.exe`, and no elevation used | |
| [ ] Next invocation and cleanup | Successful later `ck` invocation and proof that it removes `ck.exe.old` | |

## 5. Release approval

No user installation is approved until the inventory gate, CI slice, and every
applicable matrix entry above passes. A degradation may replace a passing entry
only when the release authority explicitly accepts it with a scope, rationale,
mitigation, and expiry/revisit date. Blank evidence is not an accepted
exception.

| Approval condition | Evidence or ruling | Approver and UTC time |
| --- | --- | --- |
| [ ] Inventory gate passed | | |
| [ ] Required CI slices passed | | |
| [ ] macOS matrix record passed or ruled degradation accepted | | |
| [ ] Windows 11 in Parallels matrix record passed or ruled degradation accepted | | |
| [ ] Ubuntu LTS matrix record passed or ruled degradation accepted | | |
| [ ] Fedora matrix record passed or ruled degradation accepted | | |
| [ ] macOS signing/notarization status recorded; staple-pending ruling, if any, accepted | | |
| [ ] Final release approval | | |
