# Subconscious public-history rewrite — runbook draft r1

Goal (Ufuk, 2026-08-27): make the repo public WITH its evolution history —
a git-filter-repo rewrite, not a fresh-start export. Supersedes the earlier
fresh-export ruling for this repo. Reference run: SYNAPSE's rehearsed rewrite
(978→663 commits, verified clean in scratch; their flip still parked).

## Sequence (SYNAPSE's "one thing differently" applied: verifier FIRST)

1. **Build the full-history verifier before touching anything.**
   Scan every blob (`git rev-list --objects --all` → `cat-file`) plus every
   commit message against the pattern classes. It gates the audit itself —
   SYNAPSE's loopback over-redaction would have been caught at map-authoring
   instead of tree-diff. Verifier must not contain banned literals
   (fragment-assembly); machine-local seeds live in a gitignored
   `.banlist.local`, never committed.
2. **Clean the TIP** (delete/redact/untrack), then derive the KEEP-LIST from
   `git ls-files` of the cleaned tip. Keep-list, not ban-list: everything not
   kept vanishes from all history by construction — the 193 deleted
   `.cortexkit/alfonso/**` blobs, renames, forgotten spikes, all covered
   without enumeration.
3. **String scrub rides on top**: `--replace-text` AND the same map as
   `--replace-message`. Subconscious-specific: commit messages carry
   bg_/ct_/wi_ task ids in places; the one known PII blob (personal email in
   a ck.rs fixture, tree-scrubbed at 301ca4e9) gets a blob edit; audit
   decides LAN-IP/session-id classes with seeds per class, not specimen grep.
4. **Rewrite in a scratch clone.** ~seconds at this repo's size.
5. **Equality proof (mandatory gate):** rewritten tip TREE vs pre-rewrite
   cleaned tip tree — tree-hash compare + name-only diff. This is the gate
   that caught SYNAPSE's only real bug.
6. **Full-blob verifier re-run on the rewritten repo.** Zero hits,
   mechanically — "provably clean", not "we think we got it".
7. **Force-push, THEN flip visibility. Never the reverse.** Pre-rewrite
   history must never have been public for a moment.
8. **Post-merge restoration:** untracked-but-kept process dirs (`.cortexkit/`
   live state) are removed by the merge into the canonical checkout; restore
   from disk backup explicitly.

## Known scrub inventory (from docs/audits/public-flip-precheck.md + deltas)

- 193 deleted `.cortexkit/alfonso/**` blobs → keep-list kills the class.
- One PII blob (email in fixture history) → replace-text entry.
- Whole-history secret scan was CLEAN at audit time (no keys/tokens ever;
  JWTs are test-key-1 vectors); months of commits postdate it → re-run.
- ~7k home-path blob lines: LEAVE (cosmetic, name is on the repo).
- docs/ triage: RULED (Ufuk, 2026-08-27). DROP bucket A —
  `research/competitor-capability-survey.md`, `cortexkit-grand-view.md`,
  `alfonso-app-design.md`, `specs/cortex-app-design.md`, `team-mode/`,
  `evidence/` — relocated to a private archive before the rewrite so the
  keep-list kills their history by construction. KEEP bucket B —
  hunting-loop-briefing, fleet-map/manifest/surface, OSS surveys, and
  (verified by read after the ruling) `research/grok-build-learnings.md`
  (analysis of an Apache-2.0 public repo) and `explainer/` (a self-contained
  HTML landscape explainer). Everything else keeps.
- LICENSE lands in the rewrite tip: MIT, holder "Ufuk Altinok" (rule 15401),
  license fields on the publishable crate manifests.

## Blast radius on SHA rotation (subconscious-specific)

- **Tree hashes survive; commit SHAs do not.** BROCA's SUBC_COMPAT subtree
  pins and cerebellum's declared subtree hashes compare TREE-ish — unaffected
  if the equality proof holds. Anything pinning COMMIT SHAs breaks: E2E
  frozen-set pins, cross-repo pin-ancestry checks, my own docs' SHA citations
  (become archaeology — acceptable), fleet-pulse deploy baselines.
- **AFT search index** is keyed on the commit-SHA set → identity change +
  cold rebuild; warn AFT for the window.
- **Worktree pools / open branches:** mason worktrees and any unmerged branch
  ride old SHAs → land the flip in a PR-quiet window, iceteaSA warned and
  their open work rebased by ceremony.
- **refs/pull/*:** force-push does NOT delete GitHub's PR refs. Before the
  flip, enumerate every historical PR for sensitive-path diffs (ours came via
  twin branches; iceteaSA's via fork). If any PR ever carried a
  to-be-scrubbed path, that needs GitHub support / recreation of the repo —
  measure first, assume nothing.
- **Commit count shrinks visibly** (SYNAPSE: 978→663): commits touching only
  dropped trees are pruned. Correct semantics for "evolution of the kept
  tree" — but the shrink is user-visible and should be expected, not
  discovered.

## Dependency chain (measured 2026-08-27)

Synapse's external siblings are exactly commons (already public) and
subconscious — nothing else (Cargo path-deps + CI checkout enumeration).
Subconscious is therefore the ONLY gate on synapse's flip; Ufuk has ruled
both go public. Plan: one coordinated window — subconscious rewrite+flip
first or same-window as synapse's (their stated preference), one fleet
re-clone ceremony, one AFT re-index, iceteaSA warned once.

SYNAPSE rehearsal-2 lessons folded: their only real bug was an over-broad
generic ip:port regex eating loopback examples in surviving files — the fix
was DELETING the generic class (private-LAN + provider-host classes cover
real exposures); replace maps should enumerate narrow classes, never generic
shapes. Their gates: rewritten tip tree byte-identical to canonical master +
zero hits across all blobs and messages.

## Open decisions

1. Timing vs iceteaSA's open PRs and the E2E campaign pins.
2. Private archive home for bucket A (nested-git vs subconscious-private).
