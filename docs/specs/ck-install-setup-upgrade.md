# ck install / setup / upgrade — alpha distribution for subc + AFT (+MC)

Status: DRAFT r1 — awaiting operator read. Decisions marked UFUK are his calls
with my default stated; everything else is proposed-normative.

## Flow (foundryup-shaped, one manager)

```
curl -fsSL <install-url> | sh        # macOS, Linux, WSL
irm <install-url>/win | iex          # Windows, native PowerShell
ck setup                             # wiring: config, service, PATH, harness hookup
ck upgrade                           # later: graceful binary updates
```

Unlike foundry (foundryup installs tools, foundryup manages versions), `ck` is
both the tool and the manager: the script installs ONLY `ck`, and `ck setup` /
`ck upgrade` own everything after. One binary owns the lifecycle; the script
stays ~100 dumb lines per OS and never grows logic.

Windows is first-class native PowerShell — explicitly better than foundry's
Git-Bash/WSL footnote, because two of our first alpha users are on Windows.
Test matrix before any user: Parallels Windows 11 + two Linux flavors
(Ubuntu LTS, Fedora) + macOS.

## ck setup (idempotent, re-runnable, verb not script)

1. Detect platform + existing state (fresh / partial / already-wired / standalone).
2. Fetch + verify companion binaries for the enabled set (daemon, ck-subc-mcp,
   ck-aft; MC optional) from GitHub releases — sha256 against release manifests,
   placed with the house ladder (temp+mv, warm-exec destination inode).
3. Write minimal `subc.jsonc` (daemon + aft module + mcp policy) — never
   overwrite an existing config; additive merge with a printed diff and refusal
   on conflicts.
4. Register the service: launchd agent (macOS), systemd user unit (Linux),
   Windows service (sc.exe / windows-service crate path — daemon already
   builds and tests on Windows CI).
5. Standalone conversion offers (UFUK ratified): detect standalone AFT
   (NDJSON transport) and standalone MC; OFFER conversion to subc-supervised
   mode with a printed plan, never convert silently. MC conversion respects
   the standalone-MC-user contract (MC works without subc; conversion is
   strictly opt-in).
6. Acceptance is the existing instrument set: `ck daemon triage` (exit-coded),
   `ck health`, `ck fleet lint`. Setup ends by PRINTING what it verified plus
   the one snippet to paste into Claude Code / OpenCode for the MCP shim.
7. `ck setup --uninstall`: service dereg, PATH unlink, binaries removed,
   config + stores left in place with a printed note (data is the user's).

Failure posture: every step prints its refusal explicitly and exits non-zero
(no decorative gates); partial setup is resumable because every step is
check-then-act idempotent.

## ck upgrade (manual, graceful — auto-update deliberately not built)

UFUK's shape ratified: notification + manual `ck upgrade`.

- Check: compare installed versions (daemon catalog build info + binary
  --version self-reports) against GitHub latest releases for subc / aft / mc.
  TTL-cached (24h) check runs opportunistically when the USER invokes `ck`
  (never from the daemon — the daemon stays state-free and never phones home);
  bare-`ck` dashboard gains one line: `updates: aft 0.55.0 available (ck upgrade)`.
  Offline or rate-limited check degrades silent-honest (says "not checked",
  never blocks).
- Upgrade: the productized deploy ladder we already run by hand — download,
  sha-verify against release manifest, rollback copy, temp+mv place, warm-exec
  destination, then GRACEFUL restart through the daemon's own drain machinery
  (supervisor.restart with 30s drain; module restarts absorbed by client
  retry patience; daemon restart last, via service manager, with
  initiation-ack semantics). Post-verify: pid/inode match + health + version
  self-report; automatic rollback offer on a failed post-verify.
- Ordering: modules first, daemon last (a new daemon understands old modules
  by skew-tolerance; upgrading daemon first risks stranding module HELLOs on
  a version the modules predate is NOT a real hazard today — lenient parse —
  but modules-first keeps every step's blast radius one process).
- Channels: alpha reads GitHub latest per repo. When the fleet release
  machine (docs/specs/fleet-release-machine.md) lands, `ck upgrade` becomes a
  consumer of its published channel manifests — same verb, better source.

## macOS signing (MC's live path, answered from their workflow)

Canonical reference: magic-context `.github/workflows/dashboard-release.yml`
(cert import ~L140-165, notary env ~L180-190). Adapted for a CLI binary set:

- Cert: `Developer ID Application`, imported per-run from base64 PKCS12 into
  a THROWAWAY keychain; the workflow HARD-FAILS if only `Apple Development`
  resolves (keep MC's guard — it caught a real mis-provisioned secret).
  Identity NAME resolves at runtime, never hardcoded.
- Notary auth: App Store Connect API key (issuer + key id + .p8 body written
  to $RUNNER_TEMP per run), never Apple-ID auth. Custody: repo secrets today;
  claustrum custody with per-run minting is the fleet end-state — MC will
  migrate to it when it exists.
- CLI shape (differs from MC's app): bare Mach-O binaries CANNOT be stapled.
  Sign all five with the SAME identity in one run (mixed identities reject
  the whole submission — MC's scar), hardened runtime + timestamp
  (`codesign --options runtime --timestamp`), `ditto -c -k` zip, `notarytool
  submit --wait` (2-5 min typical, degrade path: submit-succeeded/staple-
  pending on timeout), distribute binaries unstapled — Gatekeeper fetches
  the ticket online at first run. Offline-verifiable install would need a
  stapled .pkg; deferred until someone needs it.
- Quarantine reality: curl-written files carry no quarantine xattr, so the
  happy path never hits Gatekeeper; notarization is for the MDM/security-
  tooling story and browser-download paths, not the curl flow. The script
  therefore needs NO quarantine-strip line — delete that hack from r1.
- House rule kept: ad-hoc dev binaries and notarized release binaries are
  different artifacts; the dev identifier never flows into the release path.

## Settled by operator (2026-08-29)

1. Install URL: `cortexkit.io` (already in Cloudflare). Endpoint shape:
   `curl -fsSL https://cortexkit.io/install | sh` and
   `irm https://cortexkit.io/install/win | iex` — a static route serving the
   script (Worker or Pages), script content versioned in this repo; the
   route deploy ceremony rides Cloudflare access (operator or CALLO's
   worker path). The script itself stays repo-canonical so the domain is a
   mirror, never a fork.
2. Components are FIRST-CLASS: `ck setup` installs subc core and OFFERS
   detected extras; any component installs later via `ck setup aft` /
   `ck setup mc` (and `--with aft,mc` non-interactive). Only-subc,
   only-aft, only-mc, and later-addition are all supported states; the
   standalone-conversion offer is the same code path as a later add.
3. Update-notification surface: bare-`ck` dashboard line + explicit
   `ck upgrade --check`. No footers on other verbs, no OS notifications,
   no agent-lane nagging in alpha.
