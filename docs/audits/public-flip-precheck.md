# Public-flip pre-check — 2026-08-17

Scope: can `cortexkit/subconscious` go public IN PLACE (history included)?
Method: one-pass scan of EVERY blob ever committed (`git cat-file
--batch-all-objects`, 2,307 commits, 110 MiB pack), file-ever-added census,
current-tree greps, docs inventory. Every claim below is from those scans, not
recollection.

## Verdict: DO NOT flip in place. A fresh public export is safe and cheap.

The blocker is not credentials — none exist anywhere in history. The blocker is
that this repository is two things in one: the subc codebase (publishable) and
the fleet's operational journal (never publishable, and burned into history).

## What the scans found

### Credentials / key material: CLEAN (whole history)
- No private keys (`BEGIN … PRIVATE KEY`: zero across all blobs).
- No GitHub/Anthropic/OpenAI/Slack/AWS token shapes, no `ckh_` handles.
- All embedded JWTs decode to `kid: test-key-1` — deliberate conformance
  vectors, test-signed. All 64-hex strings live in test fixtures/vectors (lab
  keys, wire vectors).
- Production device identifiers (Galdor transport key, sealing key, APNs
  token, Mac transport key): zero hits in any blob ever.

### PII: one current item (fixed), heavy in history
- Current tree: personal email lived in one `ck.rs` quota-render fixture —
  scrubbed to `operator@example.com` this pass. Tracked tree now has ZERO
  `/Users/<name>` paths and zero personal emails.
- History: ~4,700 blob-lines carry the personal email and ~7,100 carry the
  home path, almost entirely in DELETED `.cortexkit/` files and superseded doc
  revisions. Unremovable without rewriting history.

### Internal material burned into history: THE hard blocker
193 `.cortexkit/alfonso/**` files were committed at various points and later
deleted — athena council outputs (48), audit evidence (40), mason prompts
(70), consult task-outputs (34), the loop ledger. Deleted-from-tree is not
deleted-from-clone: every gate verdict, internal prompt, and strategy
discussion in them is one `git log --diff-filter=D` away for any cloner.

### Current tree, policy tier (standing rule: internal docs never go public)
- `docs/` is 99 files / 3.2 MB of exactly the material the policy names:
  hunting-loop briefing (ops doctrine), fleet map / restart windows / window
  artifacts (machine topology, binary paths, module inventory), rename
  runbooks (machine-specific ops), team-mode contracts + VENDORED manifests,
  grand-view (strategy), audits, research surveys, staged-removals.
- `scripts/fleet/` is operational tooling for THIS machine.
- Infra references are benign (6 mentions of public `cortexkit.io` hosts; no
  account ids, install ids, or app ids in the tree).

## The path that works (matches the standing export policy)

1. New public repo (e.g. `cortexkit/subc`), fresh history: `crates/`,
   `clients/`, root `Cargo.toml`, CI, LICENSE, a written-for-public README,
   and a curated `docs/` subset (protocol/architecture docs only, re-read
   before export).
2. `subconscious` stays private as the ops journal; the export gains a sync
   direction later if wanted.
3. CI economics: heavy Rust matrix + release lanes move to the public repo's
   free standard runners; the private side keeps only what must stay
   (fleet scripts have no CI).
4. Pre-export gate on the new tree: re-run this audit's scans on the EXPORT
   (they are one command each), plus a human read of every doc that ships.

## If in-place public is ever forced anyway
It requires a full history rewrite (delete 193 paths + scrub PII from every
historical blob), which invalidates every clone, every pinned SHA in fleet
docs, CI caches, and the provenance skew detector's embedded commits — and
after all that, the docs/ policy tier still has to leave the tree. The export
is strictly cheaper.
