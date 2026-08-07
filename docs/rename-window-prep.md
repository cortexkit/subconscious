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

The health-dependency list is the one to check by hand. A dependency naming a module
that no longer exists may render as **permanently degraded rather than erroring**,
and that surface is a health rollup — so a stale name there becomes a red dot nobody
can clear, which is the silent direction.

`ck` in this repo renders quota output and must move with it.

## What each rename actually moves

**`claustrum` moves three identities, not one — and only one of them is a file.**
subc derives every module's database path from its id
(`<data_home>/cortexkit/<module_id>/store.db`), so the rename relocates 2.5 MB of
vault state. That much a path sweep can see. The other two it cannot:

- **The keychain service holding the master key**, derived as
  `cortexkit-credentials:<first 8 bytes of SHA-256(canonical data_dir)>`. Derived
  independently here and confirmed against the live keychain:
  `cortexkit-credentials` → `...:d9ea1ff1588e6bf6` (item **exists**),
  `claustrum` → `...:b3fa54d159e1792b` (item **not found**).
- **The admin transcript vault id**, a full SHA-256 over the same canonical path
  bytes, which is the anti-splice binding inside every admin-op MAC — derived
  independently by the daemon and the CLI from their own view of the directory.

**So moving the store alone is worse than not moving it.** Move nothing and the
vault reads as empty; move only the store and the vault is **locked** — the daemon
opens a real store, resolves the keychain under the new scope, finds nothing, and
every `credential.get` fails as `vault_locked` / `needs_reauth`. That is a
fleet-wide credential outage **presenting as a key problem rather than a rename
problem**, which is the expensive direction.

The same shape makes a half-applied rename invisible: the CLI's default data dir
follows the module id, so `ck auth` with no flags would happily **bootstrap a new
empty vault** at the new path rather than erroring.

What is *not* affected: credential handles are hashes in a table inside the store,
unrelated to the module id, so consumers' `vault-handles.json` keep working. The
audit chain and fence epoch live inside the store and travel with it — verified
after the move by `ck auth verify-audit` reporting the chain intact, never by
observing that the file has a size. The `.lease` is per-path and self-heals; do not
copy it.

**`insula` moves nothing subc owns.** It has no store under the daemon's data home
at all; its state is `~/.local/state/cortexkit/ck-quota/`, keyed on the **binary**
name rather than the module id. So the module id can move on its own, and the state
directory moves only if `ck-quota` becomes `ck-insula`.

That directory holds `redemptions.json`, the crash-safety journal that prevents
double-spending reset credits — a **fence** in the runbook's sense: it reads its own
history to avoid repeating an action, so a fresh empty one is indistinguishable
from having nothing to do, and the failure is silent double-spend rather than an
error.

**The journal is not a migration item at all**, and the reason is stronger than
binary-keying. Its path is a **hardcoded string literal** — `"cortexkit/ck-quota"`
joined onto `$XDG_STATE_HOME`, with `CK_QUOTA_STATE_DIR` as an override — not derived
from the binary name, the module id, or anything else at runtime. So renaming
`ck-quota` to `ck-insula` does **not** move it: the literal still says `ck-quota`.
Tidying that constant is an independent decision, and it belongs outside the window:
a rename that also relocates a double-spend fence is two operations wearing one name.

**Measured: the journal is `[]`.** What that rules out is specific, because of how the
records behave — a record is written only when a redemption is *attempted*
(reserve-before-POST, the reservation being the fence), resolved records are pruned
after seven days, and a **pending record is retried every 60s and never pruned**,
deliberately, since it is the only thing preventing a double spend across a crash.
So empty means *no redemption is in flight and none has resolved in seven days* — not
merely "nothing recent".

Empty is the steady state rather than a trough, because redemptions fire only near
credit expiry or at the wall. Measured alongside: soonest credit expiry is four days
out and both holding accounts are far from the wall, so a redemption in the next few
hours is unlikely — **and likely instead if the window slips past that expiry.** That
is the condition to re-check, rather than the file's emptiness as such.

## The vault rename is its own event, before the fleet window

The vault is the credential root for callosum, prefrontal, broca, plexus, synapse
and cortexkit-account, so the instant its module id changes, every one of them is
dialling a name that does not answer. Batching it with its callers means a
fleet-wide credential outage for the duration of the batch, and a failure with six
candidate causes.

Sequence, owner-specified: **daemon stopped** (the vault refuses admin writes to a
store whose lease it does not hold, and the store should be quiescent) → `cp -a` the
whole directory including `-wal` and `-shm` → **owner re-provisions the master key
under the new keychain scope** (their hands, not mine — it needs the key material)
→ start under the new id → verify by state: `ck auth status` at 21/21,
`ck auth verify-audit` chain intact, and one mint plus revoke to prove the fenced
write path and its audit append. **Then** callers flip.

Rollback: the old directory stays untouched until 21/21 is confirmed under the new
one.

The repo directory move is a **separate, later** decision. Two moves in one window
means a failure has two candidate causes.

## Prep, in the order it has to happen

1. **Ask both owners, separately from their components.** The runbook's rule: a
   component owner answers about component state, and a resident session's project
   binding is invisible to that question. The vault owner in particular restarts
   separately from the fleet.
2. **Callers ship first or ship together.** Each target const above needs a landed
   change before or in the window. A caller that ships late is a broken route, not a
   deferred rename.
3. **Re-check the redemption condition at the window**, not the file. Emptiness is
   the observable; what matters is whether a credit has come within its expiry
   window since this was written. Nothing needs moving either way — the path is a
   literal — but a redemption in flight during a restart is worth not interrupting.
4. **Compatibility symlink before the move, removed after.** Converts a hard break
   into a soft one for anything still holding the old path, including resident
   sessions that bound their project root at startup.
5. **`entorhinal` is free.** Nothing is deployed and no store exists, so it is a
   repo rename and a spec edit with no runtime component.
