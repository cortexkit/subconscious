# Configuration settings v1 samples

## Module settings reads

These JSON bodies come from the daemon's real channel-0 handlers. Frame headers
are not included. All module identities, settings and credential references are
invented. The credential reference is a reference, never a secret.

| File | Body |
| --- | --- |
| `modules.request.json` | List settings modules |
| `modules.reply.json` | Stamped module rows before activation |
| `modules.declaration-error.reply.json` | Healthy settings row beside a stored declaration that cannot be encoded |
| `schema.request.json` | Read the sample module's schema |
| `schema.reply.json` | Normalized admitted schema and digest |
| `schema-setup.request.json` | Read the invented setup module's schema |
| `schema-setup.reply.json` | Two sections carrying unchanged `x-ck-setup` annotations |
| `schema-flexible.request.json` | Read the invented module whose fields keep existing shapes |
| `schema-flexible.reply.json` | An opaque `x-ck-kind: json` field (true or an object), a scalar type list, and an optional field with no default |
| `show.request.json` | Read the complete approved settings |
| `show.reply.json` | Effective settings, sources and default/user documents |
| `show.file-error.reply.json` | Approved settings retained alongside an invalid user-file diagnostic |
| `show-default.request.json` | Read through the default layer |
| `show-default.reply.json` | Defaults and their sources |
| `watch.request.json` | Watch only the sample module |
| `watch.resync.json` | Initial complete replacement state |
| `watch.declaration-error.resync.json` | Initial state with a declaration diagnostic on just the affected row |
| `watch.change.json` | Activation changed the row's fields |
| `watch.file-error.json` | A settled user-file failure without ending the watch |
| `SHA256SUMS` | SHA-256 of every JSON sample in this directory |

The watch emits StreamData bodies. A resync may arrive at any time, including
when modules are added or removed or the stamp changes; it replaces all state.
Reopening a watch always begins with another full resync.

The setup schema groups journal reads and archive writes into separate features.
Its recommendations are root-relative field values for the wizard to propose,
not daemon defaults: the journal's `read` default remains false even though its
recommended value is true. An empty recommendation object is valid.

`file_error: {code, message}` is an optional fact separate from status. Watch
events expose a matching failure only after the save-settling grace, and omit
the member on recovery. A stamped show reply keeps its approved values and
reports the file failure immediately. Unstamped show calls with no valid live
view keep the file's refusal instead of inventing effective values or defaults.

An invalid stored declaration can be reported in the affected row's `status`
and `status_reason` fields. Readers must tolerate unfamiliar status strings.
The list and watch still include healthy rows. New declarations with an
unrepresentable number are refused at admission.

Regenerate with:

```sh
UPDATE_GOLDEN=1 scripts/test-daemon-lib.sh -- config_settings_vectors_match_real_handler_bytes
```

Without `UPDATE_GOLDEN`, the same test compares every read sample and its checksum
row byte for byte, leaving the plan samples' rows intact. JSON keys are sorted before pretty-printing, with one trailing LF,
independent of Cargo feature unification. The directory is marked `-text` so
Git never rewrites those bytes on Windows.

## Edit plans and pending plan operations

The `plan-*.json` and `apply-*.json` samples pair channel-0 request and response bodies from the
daemon's real configuration handlers. Frame headers are not included. The
examples use an invented `demo` module and settings. The module reviewer is a
fixed test reply; the daemon still checks its fields against the admitted schema
and immutable baseline. No module process is started.

- `plan-edit.json`: a user edit request and its view without refusals.
- `plan-refused.json`: an edit refused by the schema, retained in the view. Such
  a plan cannot be applied. Clients read the keyed rows at `view.refusals`, which
  is absent when empty; the digest-bound plan also retains `edit_refusals` so the
  daemon can reject an apply of a refused plan.
- `plan-get.json`: the identical pending view, retrieved by digest.
- `plan-discard.json`: dropping that pending plan without applying it.
- `plan-base-changed.json`: `config_changed_since_review` because a touched
  module's base generation no longer matches.
- `apply-unstamped.json`: apply a reviewed digest immediately on an unstamped
  install, at generation zero, without an operator prompt.
- `apply-changed.json`: `config_changed_since_review` because the user file
  changed after review. The reviewed digest is the same as in `plan-edit.json`.

There is no stamped-apply sample: the sample setup has no supervisor to reconcile
approved configuration, so a stamped apply answers `supervisor_reconcile_unavailable`. A successful stamped
apply waits for the operator's confirmation prompt; desktop callers should allow
the operator response budget (290 seconds) in addition to the control-call timeout.

The handler output contains machine-specific capture metadata. The test replaces
the scratch-root prefix in paths and encoded input keys with `/home/example`,
sets Unix device/inode decimal strings to `1`, and replaces the following hashes
with `sha256:` followed by 64 `a` characters:

- `state.digest`: the captured input's fingerprint, including file identity and
  modification time;
- `expected.digest`: the file fingerprint that an atomic write-back must match;
- hashes in `input_vector`: the combined fingerprints of a module's inputs;
- hashes in `reviewed_programs`: the identities of the programs that reviewed
  the configuration.

These hashes depend on the temporary files created by the test. The daemon
incarnation becomes `example-daemon`.
Content digests, schemas, settings, field rows, refusal rows, and write-back
intent shapes are not normalized. The test recomputes the full plan digest using
the real digest function after normalization, and uses that same digest for the
get and discard request samples. JSON object keys are sorted for consistency
across workspace feature combinations.

Regenerate on Linux from the repository root, then review the diff:

```sh
UPDATE_GOLDEN=1 scripts/test-daemon-lib.sh -- config_plan_vectors_match_real_handler_bytes
```

Without `UPDATE_GOLDEN`, the test compares every plan/apply sample and its rows in
`SHA256SUMS` byte for byte, leaving other settings samples' checksum rows intact.
To check the checksums separately, run `sha256sum --check SHA256SUMS` in this
directory. Git preserves the sample bytes on every platform.

## Workspace and project settings

These samples use the daemon's `config.settings.projects/v1` capability, with
invented projects `A`, `B`, and `C` and workspace `W`. Replies come from the
channel-0 handlers and watch/delivery workers. Most samples pair `request` and
`reply`; watch bodies and the `config.changed` push are standalone.

| File | Body |
| --- | --- |
| `project-show.json` | Full approved tuple with user, workspace and project documents and the winning source. |
| `project-show-default.json`, `project-show-user.json`, `project-show-workspace.json` | The same tuple at its three inclusive cutoffs. |
| `project-watch.request.json`, `project-watch.resync.json` | Opt in with `projects: true`; the first resync has the user row followed by project rows. |
| `project-watch.change.json` | A project's bound violation reports `file_error` on that tuple only. |
| `project-watch.drop.resync.json` | A replacement snapshot after a project leaves the registry. |
| `project-plan-edit.json`, `project-apply-unstamped.json` | Edit one workspace file and apply its reviewed digest at generation zero. |
| `project-plan-capture.json` | Review the healthy project tuple; an unavailable root is a `skipped` notice, not a blocking `refusals` row. |
| `project-plan-root-refusals.json` | Blocking `layer_symlink`, `layer_not_regular`, `layer_hardlinked`, `layer_too_large`, `layer_root_invalid` and `layer_root_ambiguous` rows. |
| `project-plan-apply-kind.json`, `project-plan-credentials.json` | Blocking `project_apply_kind_unsupported` and `credential_layer_unsupported` field rows. |
| `project-plan-unknown-workspace.json`, `project-plan-workspace-mismatch.json` | `ERROR` bodies for a missing workspace and conflicting IDs. |
| `project-plan-edit-spans-files.json` | `edit_spans_files` when an unstamped plan would write two files. |
| `project-plan-config-unavailable.json`, `project-plan-registry-unavailable.json` | `config_unavailable` for an unavailable edit root, and `project_registry_unavailable` for an enumeration outage. |
| `project-show-unknown-project.json` | `unknown_project` for an ID that is neither stored nor registered. |
| `project-changed.json`, `project-get-pending.json`, `project-applied.json` | Discovery, a pending delivery with a project-specific attempt, and its ACK. |
| `project-get-fallback.json`, `project-get-unstamped.json` | Stamped layerless fallback and fresh unstamped delivery; neither has an attempt. |
| `project-status-root-io.json` | `config.status` while one root job is past its deadline; idle status omits `root_io`. |
| `project-status-file-errors.json` | `config.status` reports an unstamped project's refused settings file without the watch's settling grace. |

The legacy watch test compares raw resync bodies before and after adding
project layers. On the same layered state, legacy subscribers get no project
events while opted-in subscribers see project changes and a resync on drop.
The existing user-only wire samples and their checksum rows are unchanged.
Valid workspace and project targets are served; this capability does not emit
`layer_not_served`.

For the project samples, paths under the fixture's scratch directories become
`/home/example` and `/home/example/repos`. As with the user plan samples, the
daemon incarnation and file identities are normalized. Registry and edit
witness digests and project approval digests depend on those paths and
identities; their hashes are normalized too. Layer content hashes and patch
preconditions based on file bytes are preserved. The test then recomputes
the normalized plan digest and uses it in the matching apply sample. The
status sample clears `ungated_inputs` to exclude the build runner's environment.

Regenerate on Linux from the repository root. Run the two writers serially
because both update the checksum manifest:

```sh
UPDATE_GOLDEN=1 scripts/test-daemon-lib.sh -- \
  project_settings_vectors_match_real_handler_bytes \
  project_settings_watch_projects_isolate_validation_and_resync_on_drop \
  --test-threads=1
```
