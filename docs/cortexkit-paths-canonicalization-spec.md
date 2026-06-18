# `cortexkit-paths` — ProjectRootId canonicalization spec (v0, for AFT review)

**Status:** draft for AFT-Alfonso review. Seeds the shared `cortexkit-paths` crate so subc and AFT canonicalize **byte-identically by construction** (one crate, not two algorithms reconciled by vectors). Vectors below are a regression guard on the shared code.

## Scope
This crate owns exactly ONE transformation: **an already-resolved project-root path → a canonical `ProjectRootId`.** It is deliberately small and dependency-light: **no serde, no transport, no git.** Published to crates.io, cortexkit-neutral (AFT consumes it standalone pre-daemon).

Explicitly **out of scope** (layered elsewhere):
- **Workspace-root walk-up** (cwd → nearest git-root/lockfile, ignoring nested lockfiles) — **harness-owned**, shared in the plugin bridge (`@cortexkit/aft-bridge`) so OpenCode + Pi can't diverge. The crate receives an already-walked-up root.
- **RepoId** (git common-dir / root-commit) — **AFT-side** (needs git; subc keys per-path, §lease).
- **Operation-target path handling** (e.g. a file being *created* doesn't exist yet — existing-ancestors fallback) — **AFT-side**, layered on `CanonicalPath`. NOT the `ProjectRootId` primitive.

## Algorithm
`ProjectRootId::from_path(p)`:
1. `realpath(p)` (Rust `std::fs::canonicalize` semantics): produce an **absolute** path with `.`/`..`/trailing separators collapsed and **symlinks resolved**.
2. On success → `ProjectRootId(canonical_path)`.
3. On `NotFound` → **reject** with a typed `NonExistentPath` error. **No logical-normalization fallback** (avoids aliasing a root whose meaning changes when components/symlinks appear later — a root must exist at attach time).
4. Other IO error → typed `CanonicalizePath { source }`.

Equality/hash are over the canonical path bytes.

## Decided rules
- **/var ↔ /private/var (macOS):** handled implicitly — `/var` is a symlink, realpath resolves it. No special-casing.
- **Symlinked root:** resolves to its target → same id as the target.
- **Case-folding:** **none, explicitly.** Rely on realpath's stored-case output. Since subc and AFT both re-canonicalize the same existing dir, they get identical stored case for free; do NOT additionally lowercase. (Missing paths are rejected, so no case ambiguity there.)
- **Git linked worktrees:** **distinct ids**, by distinct canonical PATHS. The crate does NOT parse git; a linked worktree has its own checkout directory → its own canonical path → its own id, while alternate spellings of either still converge.

## Parity contract (what the vectors guard, and what they don't)
- **Load-bearing:** cross-plugin **walk-up parity** (OpenCode + Pi must reach the SAME directory) — guarded by vectors against the shared bridge walk-up.
- **By construction (not vector-dependent):** subc ↔ AFT canonicalize identically — same crate.
- **NOT load-bearing:** TS↔Rust realpath pixel-parity. subc AND AFT **re-canonicalize the received root authoritatively** at the boundary, so even if a TS realpath mirror drifts, subc/AFT converge the same dir to the same id. (The plugin keeps its own TS canonicalization **internally self-consistent** — same fn for `projectHash` and bridge-routing so its port files agree — but that never has to byte-match the Rust crate.)

## Seed test vectors (input → expected `ProjectRootId`)
Given a real existing dir `R` (canonical `Rc`):
| # | input | expected |
|---|---|---|
| 1 | `R` | `Rc` |
| 2 | `R/` (trailing sep) | `Rc` |
| 3 | `R/.` | `Rc` |
| 4 | `R/sub/..` | `Rc` |
| 5 | symlink `L -> R`, input `L` | `Rc` |
| 6 | main checkout `M` vs linked worktree `W` of one repo | `Mc` ≠ `Wc` (distinct) |
| 7 | non-existent `R/missing` | `Err(NonExistentPath)` |
| 8 | (macOS) `/var/<existing>` | `/private/var/<existing>` |
| 9 | `R/SUB` vs `R/sub` on case-insensitive FS, `SUB` the on-disk case | both → `Rc/SUB` (realpath stored case; no fold) |

## Open for AFT
- Confirm vector #9's exact expectation on your case-insensitive test envs (we expect realpath returns the on-disk stored case; flag if your platform observations differ).
- Confirm the `NonExistentPath` reject is right for **all** ProjectRootId callers on your side at attach time (operation-target/create-file handling stays your layer atop `CanonicalPath`).
- Any additional edge case from your ad-hoc canonicalization consolidation (P0) that should become a seed vector.

---

## DELTA 1 — Windows verbatim prefix (DECIDED, in-crate, `#[cfg(windows)]`)

`std::fs::canonicalize` returns the **verbatim extended-length** form on Windows. The crate MUST normalize it to **non-verbatim** before producing the id — otherwise subc, AFT, and the harness walk-up all diverge on Windows and the by-construction parity breaks exactly where it is hardest to debug, and the verbatim prefix leaks into anything that keys/compares/displays the id:

- `\\?\C:\foo`  →  `C:\foo`
- `\\?\UNC\server\share`  →  `\\server\share`

This lives in the SHARED crate (NOT an AFT post-process), or subc/AFT diverge on Windows. Reference impl: AFT's `windows_non_verbatim_path` (`crates/aft/src/inspect/oxc_engine/resolver.rs`, mirrored in `lsp/position.rs`) — fold it in as the canonical implementation. The crate is Windows-correct from v0 (AFT needs it standalone pre-daemon).

Additional seed vectors:

| # | input | expected |
|---|---|---|
| 10 | (Windows) `\\?\C:\<existing>` | `C:\<existing>` (verbatim stripped) |
| 11 | (Windows) `\\?\UNC\server\share\<existing>` | `\\server\share\<existing>` (verbatim UNC stripped) |

## Resolution status (AFT review — converged)

- **DELTA 1 (Windows verbatim):** folded in above; AFT's `windows_non_verbatim_path` is the reference impl; lives in the shared crate, Windows-correct from v0.
- **#2 NonExistentPath reject:** CONFIRMED correct for ProjectRootId at attach (a project root always exists at configure/attach). AFT's current root canonicalization uses a `canonicalize_or_normalize` fallback; switching ProjectRootId to reject is the right tightening. The normalize fallback stays AFT-side on `CanonicalPath` for operation-target/create-file paths.
- **#9 case-insensitive stored-case:** matches AFT's assumption (realpath stored case, no fold). Provisionally confirmed; AFT adds a live-verified case-collision vector when the crate lands.
- **#3 other edges:** none beyond Windows-verbatim. AFT routes its ~10+ scattered root-identity canonicalization sites through this crate during P0; operation/relative-path sites stay the AFT-side `CanonicalPath` layer.

**Converged → crate creation proceeds.** (The "Open for AFT" section above is now resolved by this block.)
