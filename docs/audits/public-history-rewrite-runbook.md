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

## r3 — SYNAPSE eight-finding review folded (measured for subconscious)

0. **Bundle backup is STEP 0**: `git bundle create ~/Backups/subconscious-preflip.bundle --all` — the recovery path post-force-push AND the permanent resolver for every old-SHA citation. No recovery path existed in r2.
1. **Mirror-class push, never `push -f master`**: any surviving un-rewritten remote ref re-exposes the whole old history through ancestry. Measured: 3 remote branch heads, 41 tags — push all rewritten refs, DELETE the rest, verify with `git ls-remote` after.
2. **Actions history goes public at flip**: 2,391 runs whose logs carry months of env dumps and paths. Delete ALL pre-flip runs (`gh api DELETE` per run) BEFORE the visibility change; artifacts die with runs.
3. **Releases pin pre-scrub trees**: 5 subc-core releases exist and their auto-generated source archives survive force-push. Delete and recreate on rewritten tags, re-uploading the SAME binary assets (binaries are clean; the poisonous part is the frozen source tarball). AFT's CI pins a subc-core release binary — asset names and bytes must survive the recreation or AFT CI breaks.
4. **Issue/PR TEXT is its own surface**: bodies and comments go public — grep them against the pattern classes, separately from diffs.
5. **The fork is the disease**: iceteaSA's fork (measured: exactly 1 fork) retains the entire pre-scrub history regardless of what we push. Ceremony: their cooperation to delete and re-fork post-rewrite; GitHub support detach as fallback.
6. **GitHub GC lag**: old SHAs stay fetchable server-side for a while even after a perfect mirror push — the rewrite is hygiene, not revocation; nothing real needs rotation here (whole-history secret scan clean).
7. **LICENSE parity check runs both ways** — our flag caught synapse's own missing LICENSE; each repo in the window re-checks the other's tip.

## EXECUTED 2026-08-27 — repo is public

Ceremony ran start-to-finish in ~2h: bundle step-0 (6MB, ~/Backups) → bucket-A
relocation (nested-git docs/private) → tip publicized (LICENSE/README/crate
license fields) → verifier over 4,576 blobs (narrow classes; generic classes
deliberately deleted per the rehearsal-2 lesson — fake test IPs and opaque
task-id citations stayed) → filter-repo (2644→2496 commits, keep-list 453
files, ONE replace entry) → gate 1 tree BYTE-IDENTICAL → gate 2 zero hits,
zero dropped-path files → PR-paths check (no PR ever carried a dropped path)
+ issue/PR text grep clean → assets preserved then releases recreated
byte-identical (AFT pin verified by sha diff) → 2,393 runs deleted →
explicit-refs push + stale deletion, ls-remote verified exact → 19 stale twin
branches pruned → visibility flip (NOTE: the first gh edit call timed out
silently leaving the repo private — verify flips with a separate read plus an
unauthenticated curl 200, never the edit's exit code) → canonical reset
(tree-identity made it invisible to sibling consumers) → all-clears to
SYNAPSE (GO), iceteaSA (#73 re-fork + rebase), AFT (re-index + asset window).

Accepted residual, dispositioned not ignored: GitHub retains refs/pull/* and
the fork's history until iceteaSA re-forks and server GC runs. The scrubbed
classes are strategy docs, not credentials — hygiene, not revocation, per
this runbook's own rule.

## Ceremony verbs under the gh shim (v9+)

Use gh's NATIVE verbs for the two destructive steps — they are argv tuples the
shim classifies admin-tier, refusing with the operator remedy named:
`gh run delete <id>` (never raw `api -X DELETE`, which stays
unclassified-refused by design: v1 api_rules are GET-only, and an unenforceable
DELETE row would merely look adjudicated) and `gh repo edit --visibility`.
Operator executes both under GH_SHIM_BYPASS=operator — refusal-by-design, not
unclassified-by-accident. Flip verification stays three-arm: authenticated
read, unauthenticated curl, ANONYMOUS CLONE (the only arm proving an outsider
receives bytes).
