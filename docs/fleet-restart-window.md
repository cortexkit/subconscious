# Fleet restart window — plan

Written 2026-08-06. Everything below was measured from the running fleet and the
repositories on this machine, not recalled.

## Why one window

Thirteen of fourteen supervised binaries are behind their masters, most by
250-300 hours. That is not neglect: every restart of a supervised module has been
held for the owner's approval, so the backlog is the boundary working as
intended. It does mean the cheapest way to clear it is one coordinated window
rather than fourteen individual ones.

Two data migrations and one module rename also need doing, and both migrations
want the same quiescent state a restart already produces.

## Measured state

Supervised binaries behind their own master:

| binary | behind | unshipped runtime files |
|---|---|---|
| `ck-subc-mcp` | 313h | 17 |
| `ck-mc` | 287h | 3 |
| `ck-callosum` | 281h | 13 |
| `ck-plexus` | 272h | 15 |
| `ck-aft` | 270h | 32 |
| `ck-engram` | 263h | 8 |
| `ck-credentials` | 436h | 5 |
| `ck-thalamus` | 54h | 18 |
| `ck-subc` (daemon) | 264h | 12 |

**Measured directly rather than inferred:** nine module stores are currently
`0644`, including every `-wal` and `-shm` sibling; three are already `0600`. The
three correct ones are the modules whose binaries already carry the fixed crate,
which makes **the file mode a direct observation of whether the fix is running**
— better than trusting that a rebuild carried it. The WAL sibling is the
load-bearing half: recently committed rows live there before checkpointing, so a
permissive WAL exposes the newest data while the `.db` file reads correct. A
check on `store.db` alone passes on all nine.

Backup copies keep whatever mode they were taken with and no reopen fixes them,
so they are swept separately rather than counted in a fleet-wide result.

Behind the shared `commons` crates, which carry that fix: `ck-astrocyte`,
`ck-credentials`, `ck-engram`, `ck-mc`, `ck-plexus`, `ck-callosum`,
`ck-subc-mcp`, `ck-thalamus`. **Those get the fix for free from any rebuild** —
no separate step, which is why store permissions is not its own line item below.

Non-supervised binaries also stale and worth deciding on rather than rebuilding
by reflex: `broca-session`, `ck`, `ck-account`, `ck-plexus-admin`, `subc-probe`
(553h — likely dead, check before rebuilding).

`ck-broca` is current: v0.3.20 deployed and verified by inode earlier today.

## The three pieces of real work

**1. Broca state-directory move.** Its runbook is settled and reviewed. Requires
the module stopped, an atomic same-filesystem rename, and its own verify legs
(the WAL comparison against a baseline taken from the stopped tree). Independent
of everything else — confirmed by checking that no other module carries a
descriptor at the path the backup discovery reads.

**2. The executive module rename.** Old ids retire, new ids spawn. `rescan` can
now do this in one call without a daemon restart, but the store move cannot ride
inside it, so the sequence is: stop the old modules, move the stores, edit the
daemon config, dry-run, execute. The gating consumer is the desktop app, whose
module ids are compiled literals.

**3. Engram restage.** Fifteen commits, three load-bearing: the fix for the path
that stranded the account twice, the fix that reports the real refusal instead of
a fallback's symptom, and the self-heal that recovers without a human. Module
only — the worker half needs its own reviewed gate.

## Ordering, and why

0. **The credentials vault restarts first, alone, and is then excluded from the
   bounce.** Its owner ruled this rather than me: the vault is a dependency of
   several modules, so it gets its own verification and its own rollback
   decision before anything else moves. If its acceptance fails, the window does
   not proceed.
1. **The backup module restages first, and its drain runs inside the window
   rather than before it.** This inverts what the plan originally said.

   The drain was the gate for hours, on the assumption it would finish on its
   own. It cannot: the running binary mints a fresh identifier on every publish
   attempt, checked against a rule requiring it to match the first attempt's, so
   every retry is refused **by construction** rather than by bad luck. Three
   generations sit behind that wall.

   So waiting was waiting for something that could never happen. The fix ships
   in the restage, which means the restage has to lead and the drain follows it.

   If the drain does not start moving within a few minutes of the restart, that
   is a signal rather than a reason for patience — this fix either works
   immediately or does not work.
2. **Stage every new binary while modules keep running.** A remove-first copy
   leaves the running process on its old inode, so staging is not a restart. This
   front-loads all the risky building outside the outage.
3. **Each owner verifies their own staged artifact** before the window: version
   probe, warm exec, whatever symbol check they specify. An owner is the only one
   who knows what discriminates their build — and, just as usefully, what does
   *not*. Owners have so far ruled out a version string identical across both
   builds, a catalog count identical before and after, and a marker that reads
   zero in old and new alike. Each of those would have passed against a stale
   binary.
4. **Stop broca, move its state, leave it stopped.**
5. **Stop the executive modules, move their stores, edit the daemon config.**
6. **One daemon bounce.** Everything comes up on new binaries with new config.
7. **Verify by inode, per module.** Then each owner runs their own acceptance,
   including at least one mutating call — a read-only health route proves the
   service is serving and nothing about the write path.
8. **Sweep every store, its `-wal`/`-shm` siblings, and the module's `.lease`
   for `0600`.** Nine must flip; the three already correct are the control
   proving the check can read a correct state.

   The lease is the other half of the same fix and carries a *different* risk.
   A readable store is a confidentiality problem; the lease is the single-writer
   fence, so anything able to write it can forge the epoch readers use to detect
   a stale writer — an integrity problem. Both are world-readable today.

   Scope the sweep to supervised module directories. Thousands of lease files
   exist under scratch rigs and other tooling, two of those rigs are dead, and
   folding them in makes the result noisy enough to hide a real regression.

Health rows are not uniformly trustworthy immediately after a restart, and the
reason differs per module. One owner computes their status fresh at probe time,
so a disagreement between the supervisor's row and a fresh probe is the
supervisor's cache. Another serves a cached snapshot by design, so their row is
eventually-consistent for roughly fifteen seconds and a live call is the truth.
Ask per module rather than assuming one shape.

Steps 4 and 5 are the only ones with data motion, and both are separated from
the id change deliberately: if a spawn fails afterwards, the stores are already
moved and consistent, so recovery is fix-config-and-rescan with no data motion.
The partial state is boring by construction.

## What could go wrong, and the answer

- **A new binary is broken.** Every previous binary is backed up beside its
  replacement, and step 3 is where this should surface rather than in the window.

  **Rollback is not uniform, and the general case is wrong for one module.** For
  most, the previous binary reopens a migrated store cleanly — verified at the
  migration runner, which skips anything at or below the applied maximum and
  never refuses a store newer than the binary knows. So rolling back means
  replacing the binary and nothing else.

  The context module is the exception: its migration is **one-way**, and the
  previous binary will refuse the migrated store afterwards. That refusal is
  correct and loud, but it means the rollback unit there is **binary plus
  store**, restored from the pre-migration backup rather than the binary alone.
  A plan that is right for thirteen modules and wrong for one is worse than no
  plan, because the operator applies the general case under pressure.

  Every pre-migration backup is created `0600` at the moment it is taken. A copy
  inherits the mode of its source, nothing ever opens a backup, and so the
  open-time permission fix can never reach it — a backup taken minutes before
  the fix goes live would otherwise stay world-readable forever, beside a fleet
  measured as clean.
- **The daemon comes up and a module does not.** Its previous binary is one copy
  away, and the daemon keeps supervising the rest.
- **A store move fails.** Both migrations verify the destination before the
  source is removed; neither deletes anything in the window.
- **The rename spawns nothing.** Old ids gone, new ids absent, stores already
  moved. Fix config, rescan again.

## Not in this window

The worker half of engram (needs a separate reviewed gate with the credentials
owner), and the protocol change that would close the multi-device case of the
credential deadlock. That one is not "later" — it is **before a second device is
enrolled**, which is a date rather than a priority, and the phone work sets it.
