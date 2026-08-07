# Rename window prep

Three renames are queued for the next restart window. This page records what was
measured beforehand, what each one moves, and the one way this batch differs from
the last one.

Procedure lives in `docs/module-rename-runbook.md`; this is the per-rename state
and ordering, not a replacement for it.

## The batch

| from | to | binary | status |
|---|---|---|---|
| `cortexkit-credentials` | `claustrum` | `ck-claustrum` | ratified; owner not yet asked |
| `ai-provider-quota` | `insula` | `ck-insula` | agreed with owner; journal migration is the blocker |
| `ck-projects` (repo only) | `entorhinal` | — | never deployed, nothing running |

`hippocampus` is deliberately **not** used here — it is held for magic-context's
Rust cutover, where that store path can move under a migration risk that already
exists rather than as its own event.

## This batch inverts last time's deploy order

The `alfonso-core` rename was gated on **allowlists**: other modules held the old
name in a list of *who may call me*, so every holder had to accept the new name
**before** the module could adopt it, or the executive would have silently lost
bash, browser control and connector invocation.

**These two are the opposite.** Every cross-repo reference found is a **target** —
the id a caller *dials*, held in a `const` such as `CREDENTIAL_MODULE_ID` or
`QUOTA_MODULE_ID`. A target reference does not gate the rename; it **breaks at the
moment the rename lands** and stays broken until the caller ships. So callers flip
*with* the module, not before it, and every un-shipped caller is an outage for that
route rather than a latent risk.

Live callers holding a target const, verified by sweep across all 35 repos with a
`.git` entry (symlinks resolved, dead husks excluded):

- **`cortexkit-credentials`** — callosum, prefrontal (`CREDENTIAL_MODULE_ID`), broca
  (`CREDENTIALS_MODULE_ID`), plexus, synapse, cortexkit-account.
- **`ai-provider-quota`** — prefrontal-routing (`QUOTA_MODULE_ID`), astrocyte
  (`QTA_MODULE_ID`, plus a health-dependency list naming it beside broca), commons,
  brocatui, synapse.

`ck` in this repo renders quota output and must move with it.

## What each rename actually moves

**`claustrum` moves a store, by construction.** subc derives every module's
database path from its id — `<data_home>/cortexkit/<module_id>/store.db` — so the
rename relocates 2.5 MB of vault state plus its `.lease` file. Nothing optional
about it: the daemon will hand the module a descriptor pointing at the new path,
and an unmoved store reads as an empty vault rather than as an error.

**`insula` moves nothing subc owns.** It has no store under the daemon's data home
at all; its state is `~/.local/state/cortexkit/ck-quota/`, keyed on the **binary**
name rather than the module id. So the module id can move on its own, and the state
directory moves only if `ck-quota` becomes `ck-insula`.

That directory holds `redemptions.json`, the crash-safety journal that prevents
double-spending reset credits — a **fence** in the runbook's sense: it reads its own
history to avoid repeating an action, so a fresh empty one is indistinguishable
from having nothing to do, and the failure is silent double-spend rather than an
error.

**Measured now: the journal is `[]` — empty.** That makes this the cheapest possible
moment to move it, because there is nothing to lose. It is also the reason to check
again *at the window rather than trusting this page*: an empty fence and a fence
that refilled ten minutes ago look identical in every respect except content, and
this file will not update itself.

## Prep, in the order it has to happen

1. **Ask both owners, separately from their components.** The runbook's rule: a
   component owner answers about component state, and a resident session's project
   binding is invisible to that question. The vault owner in particular restarts
   separately from the fleet.
2. **Callers ship first or ship together.** Each target const above needs a landed
   change before or in the window. A caller that ships late is a broken route, not a
   deferred rename.
3. **Re-measure the journal at the window.** Empty now; verify empty then, and if it
   is not, move it rather than reasoning about whether the entries still matter.
4. **Compatibility symlink before the move, removed after.** Converts a hard break
   into a soft one for anything still holding the old path, including resident
   sessions that bound their project root at startup.
5. **`entorhinal` is free.** Nothing is deployed and no store exists, so it is a
   repo rename and a spec edit with no runtime component.
