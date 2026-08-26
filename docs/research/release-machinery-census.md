# Fleet release-machinery census

**Scope:** measurement of the release machinery present in the fleet on 2026-08-26. This is an inventory, not a design. “NONE” means no requested `scripts/release*.*`, `scripts/stage*.*`, `Makefile`/`justfile` release target, or release/publish/tag workflow was found in that repository’s tracked entrypoint locations. It does not infer that an undocumented manual procedure is impossible.

## Denominator and method

The manifest contains 19 module rows but only 17 distinct source repositories because `prefrontal` and `subconscious` each supply two modules; `commons` is the sole build-dependency repository. `magic-context` and `commons`, the two explicitly named extras, are therefore already in the manifest-derived set. The census covers 18 distinct repositories: aft, astrocyte, broca, callosum, cerebellum, claustrum, commons, engram, entorhinal, fusiform, insula, magic-context, plexus, prefrontal, subconscious, synapse, thalamus, and wernicke. [subconscious/docs/fleet-manifest.json:4-108]

For every repository, the survey enumerated the requested file-name patterns and semantic dashboard/tag workflows, then read every found entrypoint. `Makefile` exists only in aft and contains benchmark targets rather than a release target. No `justfile` was found. [aft/Makefile:1-14]

The phase vocabulary used below is literal: **stage** means a named candidate/draft/cache handoff, **place** means copying/deploying it to a consumer location, and **resume-safe** means an explicit code path handles pre-existing remote/local state. A tag or a clean-tree check alone is not counted as a durable progress record.

## Per-repository inventory

### aft

- **Entry points:** `scripts/release.sh`, `scripts/release-gate-v049.mjs`, and `.github/workflows/release.yml`; the `Makefile` has no release target. The shell entrypoint explicitly describes local bump/commit/tag/push followed by CI test/build/publish. [aft/scripts/release.sh:10-17] [aft/Makefile:1-14] [aft/.github/workflows/release.yml:1-19]
- **Order and location:** local preflight checks release notes and announcement/version consistency, then lint, typecheck, `cargo fmt --check`, versioned publication gates, Rust tests, plugin tests, and optional Docker E2E. It syncs package versions, regenerates the schema, refreshes `bun.lock` and `Cargo.lock`, commits, builds, stages a versioned cache binary, signs it on Darwin with identifier `aft`, writes a post-sign SHA sidecar, tags, and pushes. [aft/scripts/release.sh:123-180] [aft/scripts/release.sh:196-270] [aft/scripts/release.sh:273-345] [aft/scripts/release.sh:351-405] [aft/scripts/release.sh:423-428]
- **CI phases:** the tag workflow runs strict reusable unit and E2E workflows in parallel; crates.io publish waits for both, four native builds upload artifacts, GitHub Release collects artifacts and generates checksums, npm publication waits for the GitHub release, and Discord follows both GitHub and npm publication. [aft/.github/workflows/release.yml:25-58] [aft/.github/workflows/release.yml:117-121] [aft/.github/workflows/release.yml:311-321] [aft/.github/workflows/release.yml:499-625]
- **Signing, staging, verification:** the local cache stage is post-sign hashed; the darwin-x64 CI build signs and runs `codesign --verify --strict`; the release asset job writes `checksums.sha256`. [aft/scripts/release.sh:387-405] [aft/.github/workflows/release.yml:159-189] [aft/.github/workflows/release.yml:521-541]
- **State/re-entry:** if a local or origin tag points at current `HEAD`, the script enters resume mode and pushes without re-running local mutation; a tag at another commit refuses. CI publication also treats already-existing crates/npm versions as success only in its guarded per-package paths. [aft/scripts/release.sh:37-63] [aft/scripts/release.sh:99-107] [aft/.github/workflows/release.yml:77-112] [aft/.github/workflows/release.yml:423-496]
- **Load and coupling:** CI splits unit work, E2E, and native builds into separate jobs; it further isolates the release-storm latency test onto its own Linux runner. Its unit jobs obtain a short-lived token and fetch a pinned `subc-core` release binary; missing credentials make the related suites skip rather than fetch. [aft/.github/workflows/_unit-suite.yml:208-253] [aft/.github/workflows/_unit-suite.yml:332-375] [aft/.github/workflows/_e2e-suite.yml:3-12]

### astrocyte

- **Entry points:** **NONE.** The manifest identifies astrocyte as a source-acquired module. [subconscious/docs/fleet-manifest.json:11-15]
- **Phases, state, and coupling:** no requested release entrypoint records a bump, lock refresh, tag, staging, publication, placement, notification, or resumability rule; no release-specific sibling checkout was observed in this survey.

### broca

- **Entry point:** `scripts/stage-release.sh`; there is no tag/publish release workflow. Its CI is not a tag-release workflow. [broca/scripts/stage-release.sh:1-24] [broca/.github/workflows/ci.yml:1-7]
- **Order and location:** local staging sweeps old `/tmp/broca-release-staging` files, requires declared subconscious subtree hashes and clean sibling crate paths, builds `--release --locked`, copies the binary with mode 700, ad-hoc-signs it with pinned identifier `ck-broca`, and reports a SHA. [broca/scripts/stage-release.sh:41-59] [broca/scripts/stage-release.sh:87-166]
- **Verification and state:** if a deployed binary exists, it compares versions and requires a supplied marker to discriminate; an optional control must exist in both artifacts. The stage itself is cleaned by age, but records no phase ledger and has no re-entry branch. [broca/scripts/stage-release.sh:168-190] [broca/scripts/stage-release.sh:193-234]
- **CI/coupling:** ordinary CI runs on Blacksmith Linux and Windows, checks out subconscious, commons, and (on Linux) claustrum beside broca, builds sibling daemons, and makes missing real-subc/aimock/vault legs hard failures. This is a mixed local-stage/CI-gate process; the local release leg does not publish or place. [broca/.github/workflows/ci.yml:40-97] [broca/.github/workflows/ci.yml:117-159]

### callosum

- **Entry points:** **NONE.** Callosum is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:21-25]
- **Phases, state, and coupling:** no requested release entrypoint records any release phase, state, or sibling release handoff.

### cerebellum

- **Entry points:** `scripts/release-build.sh` and `scripts/stage.sh`; no tag/publish release workflow was found. [cerebellum/scripts/release-build.sh:1-22] [cerebellum/scripts/stage.sh:1-24]
- **Order and location:** the local build exports attestation revision identifiers, validates the sibling subconscious crate-tree declarations and clean sibling source, then runs `cargo build --release --locked`. The stage script refuses a dirty tree, invokes that build, copies the binary and catalog into timestamped `$HOME/ck-stage`, signs with an Apple Development identity and pinned `ck-cerebellum` identifier, and verifies the signature. [cerebellum/scripts/release-build.sh:101-110] [cerebellum/scripts/release-build.sh:167-219] [cerebellum/scripts/stage.sh:26-64]
- **Verification and state:** after signing, it verifies artifact-reported HEAD revision and clean-tree provenance, then verifies the designated requirement names both the identifier and certificate and prints SHA/UUID. It offers placement instructions but performs no placement. No progress journal or re-entry behavior is present. [cerebellum/scripts/stage.sh:66-127] [cerebellum/scripts/stage.sh:129-137]
- **CI/coupling:** regular Blacksmith Linux CI checks out subconscious next to the repository, runs the shared Rust gate, live-daemon conformance, and nextest; it has no tag trigger. Load-sensitive macOS registration timing is explicitly outside this Linux CI path. [cerebellum/.github/workflows/ci.yml:17-72] [cerebellum/.github/workflows/ci.yml:82-144] [cerebellum/.github/workflows/ci.yml:157-163]

### claustrum

- **Entry point:** `scripts/release-build.sh`; no requested stage/tag/publish release entrypoint was found. [claustrum/scripts/release-build.sh:1-27]
- **Order and location:** it refuses a dirty tree, stamps a short revision into a locked release build, copies two binaries to `target/staged/<revision>`, prunes all but recent stages while preserving the deployed revision, and signs each with an explicit identifier before reporting revision and SHA. [claustrum/scripts/release-build.sh:29-42] [claustrum/scripts/release-build.sh:52-110]
- **Verification and state:** the script runs tests against the exact staged daemon/CLI pair, rejects unexpected debug seams, and requires a caller-provided behavioral `PROBE`; it prints a later plain-copy/after-placement instruction but does not place or publish. Revision-keyed stages and pruning are artifact retention, not a resumable phase record. [claustrum/scripts/release-build.sh:112-179] [claustrum/scripts/release-build.sh:181-215]
- **CI/coupling:** regular Linux/Windows CI floats sibling subconscious and commons checkouts, builds `ck-subc`, runs contract and endpoint checks, locked lint/tests, crash-safety seam tests, real-daemon E2E, and release-artifact assertions. Those gates share the ordinary CI matrix, not a release-tag workflow. [claustrum/.github/workflows/ci.yml:37-94] [claustrum/.github/workflows/ci.yml:100-118] [claustrum/.github/workflows/ci.yml:147-208]

### commons

- **Entry point:** `.github/workflows/release.yml`; there is no local `scripts/release*`/`scripts/stage*` entrypoint. It is the manifest’s build dependency rather than a supervised module. [commons/.github/workflows/release.yml:1-17] [subconscious/docs/fleet-manifest.json:103-107]
- **Order and location:** a `*-v*` tag invokes the reusable three-OS CI matrix (format, clippy, workspace tests) and a live Postgres test before a Ubuntu CI publish parses the tag, validates it against the crate manifest, and runs `cargo publish`. [commons/.github/workflows/release.yml:22-40] [commons/.github/workflows/ci.yml:19-45] [commons/.github/workflows/ci.yml:47-78]
- **State/re-entry:** `concurrency` keys the release on ref and SHA without cancellation; a publish failure is accepted as success only if a guarded dry-run says that exact crate already exists. No local bump/tag/stage/place/notify step is defined. [commons/.github/workflows/release.yml:10-14] [commons/.github/workflows/release.yml:64-72]
- **Coupling:** the release workflow itself has no sibling checkout; no release-specific cross-repository handoff appears in that entrypoint. [commons/.github/workflows/release.yml:22-72]

### engram

- **Entry points:** **NONE.** Engram is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:42-45]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### entorhinal

- **Entry points:** **NONE.** Entorhinal is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:47-50]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### fusiform

- **Entry points:** `scripts/release-build.sh` and `scripts/stage.sh`; no tag/publish release workflow was found. [fusiform/scripts/release-build.sh:1-22] [fusiform/scripts/stage.sh:1-37]
- **Order and location:** local build refuses a dirty tree, uses `CK_BUILD_REV` with `cargo build --locked --release`, and prints version/SHA. The macOS-only stage invokes that build, copies both binaries to timestamped `$HOME/ck-stage`, signs each using an Apple Development identity with an explicit binary-name identifier, and verifies signatures. [fusiform/scripts/release-build.sh:24-63] [fusiform/scripts/stage.sh:39-66]
- **Verification and state:** staged binaries must self-report the current full HEAD revision, then the stage prints SHA and UUID. No place/publish/notify action, durable phase record, or resume branch exists. [fusiform/scripts/stage.sh:68-104]
- **CI/coupling:** regular Blacksmith Linux CI checks out subconscious, commons, and engram siblings; locked format/clippy/test and a served-schema version discipline check run there. Live/full payload tests are deliberately unset, so the same job is not an external-service load probe. [fusiform/.github/workflows/ci.yml:17-25] [fusiform/.github/workflows/ci.yml:38-74] [fusiform/.github/workflows/ci.yml:80-116]

### insula

- **Entry points:** **NONE.** Insula is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:57-60]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### magic-context

- **Entry points:** `scripts/release.sh`, `scripts/release-e2e-docker.sh`, `scripts/release-dashboard.sh`, `.github/workflows/release.yml`, and the tag workflow `.github/workflows/dashboard-release.yml`. The main script describes bump/commit/tag/push followed by CI publish; the dashboard script is a separate `dashboard-v*` train. [magic-context/scripts/release.sh:4-18] [magic-context/scripts/release-dashboard.sh:4-21] [magic-context/.github/workflows/release.yml:1-13] [magic-context/.github/workflows/dashboard-release.yml:1-16]
- **Main order and location:** the local main train checks clean tree and rejects an existing tag, runs lint/typecheck/test/build per plugin and CLI, then host/Docker E2E and Rust hermetic E2E; it generates schema/seeds, syncs version, commits, tags, pushes, and blocks on the CI run’s status. [magic-context/scripts/release.sh:62-83] [magic-context/scripts/release.sh:156-190] [magic-context/scripts/release.sh:192-355] [magic-context/scripts/release.sh:360-427]
- **Main CI:** independent unit jobs precede Docker E2E, host behavior E2E, then Rust hermetic E2E; three npm publishes wait for those gates, GitHub Release waits for the publishes, and Discord is a best-effort final job. The Rust lane clones `commons` and `subconscious` with read-only deploy keys at default-branch depth one and reports their SHAs. [magic-context/.github/workflows/release.yml:15-50] [magic-context/.github/workflows/release.yml:365-443] [magic-context/.github/workflows/release.yml:444-498] [magic-context/.github/workflows/release.yml:596-710]
- **Docker stage/verification:** local container E2E mounts the checkout read-only, copies it to tmpfs, installs with `--frozen-lockfile`, and revalidates the manifest-derived E2E file lists across the container boundary. [magic-context/scripts/release-e2e-docker.sh:19-43] [magic-context/scripts/release-e2e-docker.sh:58-91] [magic-context/scripts/release-e2e-docker.sh:120-145]
- **State/re-entry:** the main local train fails on a pre-existing tag, so it has no local resume branch. The dashboard workflow reuses an existing release by tag or creates one draft; its matrix targets one release ID. The dashboard shell process rejects a pre-existing tag but waits for all legs, requires at least 24 assets, and only then undrafts with nonempty notes (or deliberately leaves the draft unpublished without a TTY). [magic-context/scripts/release.sh:62-66] [magic-context/.github/workflows/dashboard-release.yml:16-52] [magic-context/.github/workflows/dashboard-release.yml:164-187] [magic-context/scripts/release-dashboard.sh:85-96] [magic-context/scripts/release-dashboard.sh:256-303]
- **Dashboard signing/place:** the six-platform CI build imports a Developer ID certificate and Tauri signing key, uploads to the single draft release, then publishes the release, downloads `latest.json`, pins URLs to that tag, and deploys only that manifest to `gh-pages`. [magic-context/.github/workflows/dashboard-release.yml:66-103] [magic-context/.github/workflows/dashboard-release.yml:125-187] [magic-context/.github/workflows/dashboard-release.yml:189-276]

### plexus

- **Entry point:** `scripts/release-binaries.sh`; no requested stage/tag/publish workflow was found. [plexus/scripts/release-binaries.sh:1-25]
- **Order and location:** the local script runs a `cargo build --release --locked` for both binaries and prints each linker UUID and SHA. It does not bump/tag/sign/stage/place/publish/notify. [plexus/scripts/release-binaries.sh:27-44]
- **State/re-entry and coupling:** there is no progress record or special rerun handling. Ordinary Blacksmith CI checks out subconscious and commons and materializes their absolute developer paths with symlinks, then runs a Rust gate and locked conformance fixtures; it is not a tag release workflow. [plexus/.github/workflows/ci.yml:17-31] [plexus/.github/workflows/ci.yml:44-96] [plexus/.github/workflows/ci.yml:102-137]

### prefrontal

- **Entry points:** **NONE.** The two prefrontal module rows use one source repository. [subconscious/docs/fleet-manifest.json:72-80]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### subconscious

- **Entry points:** `scripts/release.sh`, `scripts/release-darwin-binaries.sh`, `.github/workflows/release.yml`, and `.github/workflows/release-npm.yml`. The manifest has two supervised modules from this same repository. [subconscious/scripts/release.sh:4-20] [subconscious/scripts/release-darwin-binaries.sh:1-28] [subconscious/.github/workflows/release.yml:1-20] [subconscious/.github/workflows/release-npm.yml:1-25] [subconscious/docs/fleet-manifest.json:36-40] [subconscious/docs/fleet-manifest.json:82-85]
- **Crate train:** local release validates crate/version, requires a clean tree, bumps the requested manifest if needed, locally runs fmt, locked clippy, and `cargo publish --dry-run`, commits, tags, pushes, and verifies that both branch and tag reached origin. [subconscious/scripts/release.sh:22-46] [subconscious/scripts/release.sh:90-129]
- **State/re-entry:** if the matching local tag already resolves to `HEAD`, the local script pushes the tag again and verifies it; a tag at any other commit refuses. CI crate publication retries dependency-index propagation and treats an already-published crate as success. [subconscious/scripts/release.sh:51-87] [subconscious/.github/workflows/release.yml:154-180]
- **Binary train:** `subc-core-v*` tags reuse CI verification, produce Linux x64 release binaries in CI, package per-asset SHA sidecars, create-or-reuse a GitHub release, and upload with `--clobber`. The local Darwin helper insists on current `HEAD` equal to the tag, uses `--locked`, emits matching SHA sidecars, and uploads with `--clobber`. [subconscious/.github/workflows/release.yml:28-46] [subconscious/.github/workflows/release.yml:62-112] [subconscious/scripts/release-darwin-binaries.sh:30-56]
- **Npm train:** `subc-client@*` and `store@*` tags run frozen-lock install, typecheck, test, tag/version assertion, and OIDC publish. Re-publishing a version is accepted only for matching registry error forms. [subconscious/.github/workflows/release-npm.yml:10-25] [subconscious/.github/workflows/release-npm.yml:69-95] [subconscious/.github/workflows/release-npm.yml:97-117]
- **Where/coupling:** local macOS builds cover Darwin only; CI uses Blacksmith Linux for crate/npm publication and Linux binary build. The binary GitHub release is a staged handoff to sibling CI consumers, while no notification phase is defined. [subconscious/scripts/release.sh:14-20] [subconscious/.github/workflows/release.yml:35-57] [subconscious/.github/workflows/release.yml:114-122]

### synapse

- **Entry points:** **NONE.** Synapse is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:87-90]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### thalamus

- **Entry points:** **NONE.** Thalamus is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:92-95]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

### wernicke

- **Entry points:** **NONE.** Wernicke is a source-acquired manifest module. [subconscious/docs/fleet-manifest.json:97-100]
- **Phases, state, and coupling:** no requested release entrypoint records a release phase, state, or sibling release handoff.

## Comparison table

Cells abbreviate the directly cited repository sections above; `—` is absent from the documented requested release path. `L` means local shell and `CI` means a hosted workflow. “Normal CI” is recorded only where it supplies release-adjacent gates but is not tag-triggered.

| repo | bump | lock | gates-local | gates-ci | tag | sign | stage | publish | place | verify | resume-safety | notify |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| aft | L version-sync | L bun + Cargo refresh | lint/type/fmt/Rust/JS/Docker | strict unit + E2E | L `v*` | L cache ID; CI strict sign | versioned cache + post-sign SHA | crates.io, npm, GH Release | cache only | hashes, signed artifact, gate | same-HEAD tag; registry skips | Discord best-effort |
| astrocyte | — | — | — | — | — | — | — | — | — | — | — | — |
| broca | — | L `--locked` | sibling fence + marker | normal CI only | — | ad-hoc pinned ID | `/tmp` candidate | — | — | SHA, version, marker/control | age cleanup only | — |
| callosum | — | — | — | — | — | — | — | — | — | — | — | — |
| cerebellum | — | L `--locked` | sibling tree fence | normal CI only | — | Apple ID + requirement | `$HOME/ck-stage` | — | instruction only | revision/tree/signature/SHA | — | — |
| claustrum | — | L `--locked` | staged-artifact E2E + probe | normal CI only | — | ad-hoc pinned IDs | `target/staged/<rev>` | — | instruction only | staged pair + seam + probe | retained stages only | — |
| commons | external/manual | CI lock not specified | — | 3-OS fmt/clippy/test + Postgres | `*-v*` trigger | — | — | crates.io | — | tag/version + existing check | existing crate accepted | — |
| engram | — | — | — | — | — | — | — | — | — | — | — | — |
| entorhinal | — | — | — | — | — | — | — | — | — | — | — | — |
| fusiform | — | L `--locked` | dirty-tree + self-report | normal CI only | — | Apple ID + pinned IDs | `$HOME/ck-stage` | — | — | self-reported rev, SHA/UUID | — | — |
| insula | — | — | — | — | — | — | — | — | — | — | — | — |
| magic-context | L version-sync | frozen install in E2E/CI | lint/type/test/build/host+Docker E2E | staged E2E, Rust hermetic | L `v*`, dashboard `dashboard-v*` | dashboard Developer ID/Tauri | Docker tmpfs; GH draft | npm + GH Release | dashboard `gh-pages` manifest | E2E lists; CI status/assets | dashboard reuses draft; main tag rejects | Discord best-effort |
| plexus | — | L `--locked` | build only | normal CI only | — | — | — | — | — | SHA/UUID | — | — |
| prefrontal | — | — | — | — | — | — | — | — | — | — | — | — |
| subconscious | L per-crate manifest | L/CI `--locked`, frozen npm | fmt/clippy/publish dry run | reused Linux/Windows CI | crate/npm/core tags | — | CI release assets + local Darwin tarballs | crates.io, npm, GH Release | GH assets for sibling CI | origin predicates, SHA sidecars, tag/version | same-HEAD tag, release reuse/clobber, registry skips | — |
| synapse | — | — | — | — | — | — | — | — | — | — | — | — |
| thalamus | — | — | — | — | — | — | — | — | — | — | — | — |
| wernicke | — | — | — | — | — | — | — | — | — | — | — | — |

## Commonality measurements

- **No listed release phase appears in more than half of the 18 repositories.** The largest observed family is local build/lock-adjacent machinery in eight repositories (aft, broca, cerebellum, claustrum, fusiform, magic-context, plexus, subconscious); ten repositories have no requested release entrypoint at all. This counts only documented release paths, not generic development CI. [aft/scripts/release.sh:351-387] [broca/scripts/stage-release.sh:125-161] [cerebellum/scripts/release-build.sh:204-219] [claustrum/scripts/release-build.sh:39-42] [fusiform/scripts/release-build.sh:36-43] [magic-context/scripts/release.sh:156-190] [plexus/scripts/release-binaries.sh:27-44] [subconscious/scripts/release.sh:105-113]
- **Tag plus registry publication has four implementations:** aft tags locally then CI publishes crates and npm; magic-context tags locally then CI publishes three npm packages; subconscious uses separate crate, core-binary, and npm tag grammars; commons is CI-only after a manually created crate tag. [aft/scripts/release.sh:376-428] [aft/.github/workflows/release.yml:54-112] [magic-context/scripts/release.sh:378-427] [magic-context/.github/workflows/release.yml:444-604] [subconscious/.github/workflows/release.yml:3-14] [subconscious/.github/workflows/release-npm.yml:10-18] [commons/.github/workflows/release.yml:3-40]
- **Signing has six materially different forms:** aft locally ad-hoc-signs a versioned cache with identifier `aft` and CI strictly validates a Darwin build; broca pins an ad-hoc `ck-broca` identifier; cerebellum pins an Apple Development identity and validates the designated requirement; claustrum pins ad-hoc identifiers; fusiform pins an Apple Development identity per binary; dashboard imports a Developer ID certificate and Tauri key. [aft/scripts/release.sh:389-405] [aft/.github/workflows/release.yml:159-167] [broca/scripts/stage-release.sh:155-165] [cerebellum/scripts/stage.sh:63-127] [claustrum/scripts/release-build.sh:101-110] [fusiform/scripts/stage.sh:61-78] [magic-context/.github/workflows/dashboard-release.yml:125-187]
- **Candidate handoff has five forms:** aft’s versioned cache with post-sign sidecar, broca’s age-swept `/tmp` directory, cerebellum/fusiform’s timestamped `$HOME/ck-stage`, claustrum’s revision-keyed `target/staged`, and magic-context dashboard’s single GitHub draft release. Subconscious instead hands tagged platform tarballs to GitHub Release assets. [aft/scripts/release.sh:389-421] [broca/scripts/stage-release.sh:41-59] [cerebellum/scripts/stage.sh:26-64] [fusiform/scripts/stage.sh:50-66] [claustrum/scripts/release-build.sh:69-110] [magic-context/.github/workflows/dashboard-release.yml:16-52] [subconscious/.github/workflows/release.yml:75-112]
- **Re-entry is split into five forms:** aft and subconscious resume a same-HEAD tag; subconscious additionally uses asset `--clobber`; commons and the registry legs of aft/subconscious accept an already published exact version; dashboard reuses an existing release; magic-context’s main shell and dashboard shell reject an existing tag. [aft/scripts/release.sh:37-63] [subconscious/scripts/release.sh:75-87] [subconscious/scripts/release-darwin-binaries.sh:46-55] [commons/.github/workflows/release.yml:64-72] [magic-context/.github/workflows/dashboard-release.yml:34-52] [magic-context/scripts/release.sh:62-66] [magic-context/scripts/release-dashboard.sh:85-89]
- **Notification is present only in aft and magic-context main trains, and both send Discord after release/publish with `continue-on-error: true`.** Subconscious and commons define no notification job in their release workflows. [aft/.github/workflows/release.yml:560-625] [magic-context/.github/workflows/release.yml:660-710] [subconscious/.github/workflows/release.yml:114-180] [commons/.github/workflows/release.yml:22-72]

## Chartered-constraint relevance

This section maps existing evidence to the five named constraints; it does not rank, extend, or prescribe it.

- **Crash-safe:** aft retains enough tag state to resume a same-HEAD interruption; subconscious does the same and verifies that the branch/tag reached origin; the dashboard workflow atomically selects one existing-or-new release ID before its build matrix. Broca removes old staging residue, while claustrum preserves revision-keyed stages, but neither script records a phase transition. [aft/scripts/release.sh:37-63] [subconscious/scripts/release.sh:51-87] [magic-context/.github/workflows/dashboard-release.yml:16-52] [broca/scripts/stage-release.sh:48-59] [claustrum/scripts/release-build.sh:69-99]
- **Idempotent re-entry:** explicit same-HEAD re-entry exists in aft and subconscious; existing release assets are reused/clobbered in subconscious; dashboard reuses its draft; registry already-exists success exists in aft, subconscious, and commons. Magic-context’s main local script makes the contrasting measurement: existing tag is a hard error. [aft/scripts/release.sh:99-107] [subconscious/scripts/release.sh:75-87] [subconscious/.github/workflows/release.yml:102-112] [magic-context/.github/workflows/dashboard-release.yml:34-52] [aft/.github/workflows/release.yml:423-496] [subconscious/.github/workflows/release.yml:154-180] [commons/.github/workflows/release.yml:64-72] [magic-context/scripts/release.sh:62-66]
- **Load-class separation:** aft explicitly places release-storm latency tests on a dedicated runner and has separately reusable unit/E2E workflows; magic-context uses distinct CI jobs for unit, Docker E2E, host E2E, and Rust E2E but they are all hosted Ubuntu-class jobs; its local release script runs the local checks in sequence. The other staged-binary paths are local scripts or ordinary CI gates without a documented load-class lane. [aft/.github/workflows/_unit-suite.yml:332-375] [aft/.github/workflows/_e2e-suite.yml:3-12] [magic-context/.github/workflows/release.yml:15-44] [magic-context/scripts/release.sh:156-355]
- **Notification-as-contract:** aft and magic-context require curated release-note files before GitHub release and derive Discord content from those files; both announcements are final, best-effort jobs rather than publication gates. No notification phase was found for the other 16 repositories. [aft/.github/workflows/release.yml:543-625] [magic-context/.github/workflows/release.yml:608-710]
- **Cross-repo drift:** broca’s stage has a per-subtree `SUBC_COMPAT` fence because `--locked` cannot identify path dependency source; cerebellum’s local build compares declared subconscious subtree hashes; claustrum, fusiform, and plexus CI explicitly check out sibling path dependencies; magic-context Rust E2E clones floating depth-one commons/subconscious checkouts and reports their SHAs; aft fetches a pinned subconscious release binary for its unit suites. [broca/scripts/stage-release.sh:61-123] [cerebellum/scripts/release-build.sh:134-202] [claustrum/.github/workflows/ci.yml:67-105] [fusiform/.github/workflows/ci.yml:38-74] [plexus/.github/workflows/ci.yml:44-96] [magic-context/.github/workflows/release.yml:385-420] [aft/.github/workflows/_unit-suite.yml:208-253]

## Readability and clean-subject check

All 18 repositories were readable; none was omitted. Before creating this report, the required status command produced the following output. The current worktree is intentionally excluded because it contains this new report.

```text
$ for repo in aft astrocyte broca callosum cerebellum claustrum commons engram entorhinal fusiform insula magic-context plexus prefrontal subconscious synapse thalamus wernicke; do printf '%-16s ' "$repo"; git -C "/Users/ufukaltinok/Work/Projects/CortexKit/$repo" status --porcelain | if IFS= read -r line; then printf 'DIRTY %s\\n' "$line"; else printf 'CLEAN\\n'; fi; done

aft              CLEAN
astrocyte        CLEAN
broca            CLEAN
callosum         CLEAN
cerebellum       CLEAN
claustrum        CLEAN
commons          CLEAN
engram           CLEAN
entorhinal       CLEAN
fusiform         CLEAN
insula           CLEAN
magic-context    CLEAN
plexus           CLEAN
prefrontal       CLEAN
subconscious     CLEAN
synapse          CLEAN
thalamus         CLEAN
wernicke         CLEAN
```

## Citation spot-check

The following deterministic-random sample validates that the cited source file exists under `/Users/ufukaltinok/Work/Projects/CortexKit`, its requested line range is positive and ordered, and the file contains the range. The checker samples 40 citations (more than the requested 30) with seed `release-census-20260826`.

```text
$ python3 citation-check.py docs/research/release-machinery-census.md --seed release-census-20260826 --sample 40
checked=40 seed=release-census-20260826
passed=40 failed=0
```
