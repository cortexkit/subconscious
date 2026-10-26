#!/usr/bin/env bash
#
# EVERY REFUSAL PRINTS THE SAME PREFIX: "REFUSED: ". There were two vocabularies
# until 2026-09-18 -- lowercase "refusal:" from argument validation, uppercase
# "REFUSED:" from the gate arms -- and a caller filtering output with
# `grep -E "...|REFUS"` saw NOTHING when a path was wrong, because the arg-
# validation path used the other spelling. An empty filter result reads as
# quiet success, so a real refusal became invisible at the exact moment the
# operator most needed it.
#
# THE RULE THIS ENCODES: the party that knows the outcome must NAME it, because
# every downstream filter is guessing at a vocabulary. A caller's grep is an
# allow-list over outcomes and inherits the allow-list defect -- the outcome
# nobody anticipated is the one that goes silent. Corollary for readers: prefer
# a position-based view (`tail`) over a pattern-based one for verdicts, since a
# verdict you failed to predict still occupies the last line.

# THE INVARIANT THIS SCRIPT DEFENDS, STATED ONCE, EXECUTABLY NOWHERE YET:
#
#     THE THING BEING VERIFIED IS THE THING THAT WILL RUN.
#
# Every arm below is an INSTANCE of a way that sentence can be false: PATH
# resolving to a different file than the one placed; a sidecar describing
# another file; a rollback verified against itself; a marker counted in the
# wrong table; a text file that is not an executable at all. Not one of the arms
# says the sentence.
#
# BROCA's diagnostic (2026-09-19) is why that matters and it is uncomfortable:
# A GUARD THAT CATCHES STRANGERS IS DEFENDING AN INVARIANT; A GUARD THAT KEEPS
# CATCHING THE PERSON WHO WROTE IT IS COMPENSATING FOR ONE NOBODY HAS WRITTEN
# DOWN. At least three of these arms have refused MY OWN cards. I had read that
# as the gate working -- and it is, which is exactly what keeps the tell quiet:
# a gate that never fires is obviously untested and gets examined, while a gate
# that keeps catching its author reads as vigilance and gets praised.
#
# So this header is a statement of intent and a standing TODO, not a claim of
# coverage. The arms are load-bearing and each one is here because it caught a
# real defect; what is missing is a check of the sentence itself, which would
# make the next novel falsification route fail by name instead of sailing past
# five arms that were each written for a different one. If you add an arm here,
# ask BROCA's question first: what else is the same shape and would NOT be
# caught by the arm I am about to write?
#
# Place a staged module binary with every gate arm that has caught a real defect.
#
# Each arm exists because it failed once, and each failure was a TRUE statement about
# the wrong object rather than a missing check:
#   which          a --version read from the file you placed is true about that file,
#                  while PATH resolves an older one the operator actually runs
#                  (ck-models, 2026-09-14: new CLI on disk at a path nothing resolves).
#   sidecar verify a rollback nobody can verify is not a rollback, and an incident is
#                  the wrong moment to find the sidecar was written for another file.
#   marker + control  a discriminator that reads 0/0 proves nothing (it may have been
#                  dead-code-eliminated); a control that reads 1/1 proves the reader works.
#   inode          proc-vs-disk is the only proof the restarted process runs these bytes;
#                  `cp` in place preserves the inode and makes the check a tautology,
#                  so placement is always copy-to-tmp then atomic mv.
#   warm-exec      macOS first-exec assessment is per-inode and does not transfer from
#                  the staging path, so it must run on the destination before the restart.
#
#
# CUTTING THE DAEMON ON macOS: use cut-daemon.sh to bootout, wait, and bootstrap.
# bootout unloads the job, so KeepAlive does not apply while it is unloaded.
# The generated plist uses KeepAlive = { SuccessfulExit = false; }: crashes
# restart, but the daemon's deliberate SIGTERM shutdown exits 0 and stays down.
# Bare SIGKILL is therefore not a way to stop a loaded job permanently.
# Verify a restart by a new pid, inode proc == disk, and a fresh
# `subc daemon starting` line, not just by launchctl's exit code.
#
# Usage:
#   place-module.sh --module <id> --staged <path> [--dest <path>] [--path-face <name>]
#                   --marker <string> [--control <string>] [--old-control <string>]
#                   --tool-defs <unchanged|changed|none> [--host-restart-window]
#                   [--no-restart]
#   place-module.sh --module <id> --rollback <stamp> [--dest <path>]
#                   [--restore-store <path>] [--no-restart]
#   place-module.sh --module <id> --staged <path> [--dest <path>] --marker <string> --install
#                   (a module's FIRST install: no running binary, never restarts)
#
# Refuses (exit 2) before touching the destination if any pre-arm fails.
#
# MUTATION REQUIRES --place. The default runs every arm and stops before the first
# side effect, because a tool that places by DEFAULT is one distracted invocation
# from placing when you meant to test it -- which happened on 2026-09-15. The
# default should be the one that is safe to be wrong about.
set -euo pipefail

STAGING="${CK_STAGING:-$HOME/.local/share/cortexkit/staging}"
BIN_DIR="${CK_BIN_DIR:-$HOME/.local/share/cortexkit/bin}"
MODULE=""; STAGED=""; DEST=""; PATH_FACE=""; MARKER=""; CONTROL=""; OLD_CONTROL=""; GONE=""; RESTART=1; PLACE=0; OLDER=0; MIGRATES=""; NEW_REQUIREMENT=""; ALLOW_UNHARDENED=0; ALLOW_UNSTRIPPED=0; ALLOW_UNSTAMPED=0; INSTALL=0; ROLLBACK=""; RESTORE_STORE=""; TOOL_DEFS=""; HOST_RESTART_WINDOW=0

while (($# > 0)); do
  case "$1" in
    --module) MODULE="$2"; shift 2 ;;
    --staged) STAGED="$2"; shift 2 ;;
    --dest) DEST="$2"; shift 2 ;;
    --rollback) ROLLBACK="$2"; shift 2 ;;
    --restore-store) RESTORE_STORE="$2"; shift 2 ;;
    --path-face) PATH_FACE="$2"; shift 2 ;;
    --marker) MARKER="$2"; shift 2 ;;
    --control) CONTROL="$2"; shift 2 ;;
    --old-control) OLD_CONTROL="$2"; shift 2 ;;
    --gone) GONE="$2"; shift 2 ;;
    --place) PLACE=1; shift ;;
    --older) OLDER=1; shift ;;
    --new-requirement) NEW_REQUIREMENT="$2"; shift 2 ;;
    # Rolling a hardened module back to a build from before its hardening is the
    # one legitimate removal of the runtime flag, and it must be possible in an
    # incident. It is an explicit flag, never a default, and the output says what
    # it reopens.
    --allow-unhardened) ALLOW_UNHARDENED=1; shift ;;
    # An unstripped build carries its debug map, which names every source path and
    # symbol of the build machine and makes the binary several times larger. Cards
    # are built stripped; this lets an incident placement of an unstripped build
    # through on purpose.
    --allow-unstripped) ALLOW_UNSTRIPPED=1; shift ;;
    # A build that stamps no commit (it was built from a dirty tree, or outside
    # git) cannot prove which source it came from. This lets one through on
    # purpose, for an incident; the output says it was unstamped.
    --allow-unstamped) ALLOW_UNSTAMPED=1; shift ;;
    --before) BEFORE_CMD="$2"; shift 2 ;;
    # A card that MIGRATES THE STORE cannot be rolled back by binary alone: the
    # old binary meets a newer schema and refuses on store_ahead, which is the
    # correct fail-closed behaviour and also means the binary snapshot restores
    # nothing. Naming the store here snapshots it too, so the rollback is
    # BINARY + STORE. Raised by FUSI before a v5->v6 placement, after this script
    # had printed "rollback ... verified" on every migrating card it ever placed.
    --migrates) MIGRATES="$2"; shift 2 ;;
    --check-only) shift ;;  # now the default; accepted so older call sites keep working
    --no-restart) RESTART=0; shift ;;
    # A FIRST INSTALL has no running binary to compare against, so the arms that
    # read the live image (signing posture against running, marker staged/live,
    # control, rollback snapshot) cannot run. --install replaces them with checks
    # that need no live image: the destination must NOT exist yet, the staged
    # binary must be hardened with no get-task-allow, its identifier must be the
    # destination's name, it must be ad-hoc or signed by the daemon's own team,
    # and the marker must read in it. It never restarts: the module starts when
    # its subc.jsonc entry exists and a rescan or daemon cut picks it up.
    --install) INSTALL=1; RESTART=0; shift ;;
    # Agent hosts register each head's tool definitions (descriptions and argument
    # schemas) when the head starts, and core refuses a call whose provider's
    # definitions changed since then (`provider_snapshot_reregister_required`)
    # until the hosts restart. So a build that changes a definition must be placed
    # just before a host restart, never on its own. Nothing in this script can read
    # a build's definitions, so the operator states the answer explicitly, compared
    # against the RUNNING build rather than the card it replaces: `none` for a
    # module that serves no tools.
    --tool-defs) TOOL_DEFS="$2"; shift 2 ;;
    --host-restart-window) HOST_RESTART_WINDOW=1; shift ;;
    *) echo "REFUSED: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

[ -n "$MODULE" ] || { echo "REFUSED: --module is required" >&2; exit 2; }
if [ -n "$ROLLBACK" ]; then
  [[ "$ROLLBACK" =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || { echo "REFUSED: --rollback must be a UTC stamp in YYYYMMDDTHHMMSSZ form" >&2; exit 2; }
  [ "$INSTALL" -eq 0 ] || { echo "REFUSED: --rollback cannot be combined with --install" >&2; exit 2; }
  [ -z "$MIGRATES" ] || { echo "REFUSED: --rollback cannot be combined with --migrates; use --restore-store" >&2; exit 2; }
else
  [ -n "$STAGED" ] || { echo "REFUSED: --staged is required" >&2; exit 2; }
  [ -n "$MARKER" ] || { echo "REFUSED: --marker is required (a discriminator that separates this build from the running one); pass --marker none for a change that adds no literal" >&2; exit 2; }
  [ -z "$RESTORE_STORE" ] || { echo "REFUSED: --restore-store requires --rollback" >&2; exit 2; }
  if [ "$INSTALL" -eq 0 ]; then
    case "$TOOL_DEFS" in
      unchanged|none) ;;
      changed)
        [ "$HOST_RESTART_WINDOW" -eq 1 ] || { echo "REFUSED: --tool-defs changed: a tool definition change is placed only just before a host restart; pass --host-restart-window when that restart is booked" >&2; exit 2; } ;;
      "") echo "REFUSED: --tool-defs is required: compare the build's tool descriptions and schemas against the RUNNING build and pass unchanged, changed or none (serves no tools)" >&2; exit 2 ;;
      *) echo "REFUSED: --tool-defs must be unchanged, changed or none, not '$TOOL_DEFS'" >&2; exit 2 ;;
    esac
  fi
fi

# `--marker none` IS FOR A CHANGE THAT ADDS NO STRING, and it is honest rather
# than a bypass. A deletion-only fix, a removed call, a private fn that inlines
# away, or a type-level refactor leaves every literal identical: `strings` is
# then correct to report no difference, and demanding a marker would force the
# operator to invent one.
#
# IDENTITY STILL HOLDS WITHOUT IT, because the marker was never the only link:
#
#   staged sidecar verifies        the staged file is the digest the owner published
#   placed sha == staged sha       what landed is what was gated
#   running inode == disk inode    the kernel mapped that file
#
# That chain is complete on its own. What the marker adds is a second, WEAKER
# statement -- that an expected literal is compiled in -- which never proved the
# branch was reachable anyway. So its absence costs less than it appears to.
#
# What IS lost is the cross-check that the staged bytes differ from the running
# ones in the way the card claims, so the gate substitutes the strongest
# available: LC_UUID must differ. A rebuild always moves it, and two files with
# the same UUID are the same build.
#
# Raised by ENGRAM (2026-09-19) on a fix that deletes one call and adds a
# comment. They stated "no markers by strings" on the card rather than reaching
# for a literal that would have read 1/1 and looked like a passing arm.
# --module IS THE SUPERVISOR'S MODULE ID (`plexus`), not the binary name
# (`ck-plexus`). The binary name is derived from it for the default --dest and
# the currency lookup, but the restart step passes --module to `ck module
# restart` verbatim. Passing the binary name placed the bytes, then the restart
# was refused as an unknown module, pipefail aborted the script with nothing
# printed after warm-exec, and the module kept running the OLD inode (PLEX's
# 6adcb26, 2026-09-23). So a restarting placement resolves the id against the
# supervisor BEFORE anything is mutated.
if [ "$RESTART" -eq 1 ] && ! ck module status "$MODULE" >/dev/null 2>&1; then
  hint=""
  if [ "${MODULE#ck-}" != "$MODULE" ] && ck module status "${MODULE#ck-}" >/dev/null 2>&1; then
    hint=" -- did you mean --module ${MODULE#ck-}? (--module is the supervisor id, not the binary name)"
  fi
  echo "REFUSED: the supervisor does not know module '$MODULE', so the restart step would fail after placement${hint}; pass --no-restart to place without restarting" >&2
  exit 2
fi
DEST="${DEST:-$BIN_DIR/ck-$MODULE}"
if [ -n "$ROLLBACK" ]; then
  [ -f "$DEST" ] || { echo "REFUSED: destination does not exist: $DEST" >&2; exit 2; }
else
  [ -f "$STAGED" ] || { echo "REFUSED: staged artifact not found: $STAGED" >&2; exit 2; }
  if [ "$INSTALL" -eq 1 ]; then
    [ ! -e "$DEST" ] || { echo "REFUSED: --install but $DEST already exists, so this is a placement; drop --install" >&2; exit 2; }
    [ -z "$MIGRATES" ] || { echo "REFUSED: --migrates with --install: a module installed for the first time has no store to snapshot" >&2; exit 2; }
    [ "$MARKER" != "none" ] || { echo "REFUSED: --install needs a real --marker: with no running image, a string found in the staged binary is the check that the instrument read this build" >&2; exit 2; }
  else
    [ -f "$DEST" ] || { echo "REFUSED: destination does not exist, so this is an install rather than a placement (pass --install for a module's first install): $DEST" >&2; exit 2; }
  fi
fi

say() { printf '%s\n' "$*"; }
refuse() { printf 'REFUSED: %s\n' "$*" >&2; exit 2; }

FACE_STATE="not_checked"
FACE_NAMES=()
FACE_CREATE_NAMES=()
FACE_KEEP_NAMES=()
FACE_REMOVE_PATHS=()

# Read the link target rather than following the link path, then resolve it
# relative to the link's directory before comparing it with the placed binary.
face_link_resolves_to_dest() {
  [ -L "$1" ] || return 1
  local target
  target=$(readlink "$1") || return 1
  python3 - "$1" "$target" "$DEST" <<'PY'
import os, sys
link, target, destination = sys.argv[1:]
target_path = target if os.path.isabs(target) else os.path.join(os.path.dirname(os.path.abspath(link)), target)
sys.exit(0 if os.path.realpath(target_path) == os.path.realpath(destination) else 1)
PY
}

face_is_declared() {
  local name
  [ "${#FACE_NAMES[@]}" -gt 0 ] || return 1
  for name in "${FACE_NAMES[@]}"; do
    [ "$name" = "$1" ] && return 0
  done
  return 1
}

prepare_cli_face_plan() {
  FACE_CREATE_NAMES=()
  FACE_KEEP_NAMES=()
  FACE_REMOVE_PATHS=()
  [ "$FACE_STATE" = "present" ] || return 0

  if [ "${#FACE_NAMES[@]}" -gt 0 ]; then
    local ck_command reserved_output reserved name face_path link
    ck_command=$(command -v ck || true)
    if [ -z "$ck_command" ]; then
      [ -x "$BIN_DIR/ck" ] || refuse "cannot check reserved CLI face names: no ck command is on PATH and $BIN_DIR/ck is not executable"
      ck_command="$BIN_DIR/ck"
    fi
    reserved_output=$("$ck_command" --ck-cli-face-reserved-names) \
      || refuse "could not read reserved CLI face names from $ck_command"
    for name in "${FACE_NAMES[@]}"; do
      while IFS= read -r reserved; do
        [ "$reserved" = "$name" ] \
          && refuse "CLI face '$name' is a built-in ck command and cannot be placed"
      done <<< "$reserved_output"
    done
  fi

  if [ "${#FACE_NAMES[@]}" -gt 0 ]; then
    for name in "${FACE_NAMES[@]}"; do
      face_path="$BIN_DIR/ck-$name"
      if [ -L "$face_path" ]; then
        if face_link_resolves_to_dest "$face_path"; then
          FACE_KEEP_NAMES+=("$name")
        else
          refuse "$face_path is a symlink not owned by $DEST; refusing to replace it"
        fi
      elif [ -e "$face_path" ] || [ -d "$face_path" ]; then
        refuse "$face_path already exists and is not a symlink owned by $DEST; refusing to replace it"
      else
        FACE_CREATE_NAMES+=("$name")
      fi
    done
  fi

  for link in "$BIN_DIR"/ck-*; do
    [ -L "$link" ] || continue
    face=${link##*/ck-}
    face_is_declared "$face" && continue
    if face_link_resolves_to_dest "$link"; then
      FACE_REMOVE_PATHS+=("$link")
    fi
  done
}

print_cli_face_plan() {
  case "$FACE_STATE" in
    not_checked) say "cli faces: not checked; links unchanged" ;;
    absent) say "cli faces: not declared; existing links left unchanged" ;;
    present)
      local name link
      if [ "${#FACE_CREATE_NAMES[@]}" -gt 0 ]; then
        for name in "${FACE_CREATE_NAMES[@]}"; do
          say "cli face: would create ck-$name -> $DEST"
        done
      fi
      if [ "${#FACE_KEEP_NAMES[@]}" -gt 0 ]; then
        for name in "${FACE_KEEP_NAMES[@]}"; do
          say "cli face: would keep ck-$name -> $DEST"
        done
      fi
      if [ "${#FACE_REMOVE_PATHS[@]}" -gt 0 ]; then
        for link in "${FACE_REMOVE_PATHS[@]}"; do
          say "cli face: would remove ${link##*/} -> $DEST"
        done
      fi ;;
  esac
}

reconcile_cli_faces() {
  case "$FACE_STATE" in
    not_checked) say "cli faces: not checked; links unchanged"; return 0 ;;
    absent) say "cli faces: not declared; existing links left unchanged"; return 0 ;;
  esac

  local relative_target name face_path temp_link link
  mkdir -p "$BIN_DIR"
  relative_target=$(python3 - "$DEST" "$BIN_DIR" <<'PY'
import os, sys
print(os.path.relpath(os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])))
PY
  ) || refuse "could not compute a relative CLI face target for $DEST"

  if [ "${#FACE_KEEP_NAMES[@]}" -gt 0 ]; then
    for name in "${FACE_KEEP_NAMES[@]}"; do
      say "cli face: kept ck-$name -> $DEST"
    done
  fi
  if [ "${#FACE_CREATE_NAMES[@]}" -gt 0 ]; then
    for name in "${FACE_CREATE_NAMES[@]}"; do
      face_path="$BIN_DIR/ck-$name"
      if [ -L "$face_path" ]; then
        if face_link_resolves_to_dest "$face_path"; then
          say "cli face: kept ck-$name -> $DEST"
          continue
        fi
        refuse "$face_path changed to a symlink not owned by $DEST; refusing to replace it"
      elif [ -e "$face_path" ] || [ -d "$face_path" ]; then
        refuse "$face_path appeared as a non-owned path; refusing to replace it"
      fi
      temp_link="$BIN_DIR/.ck-$name.$$.$RANDOM.tmp"
      ln -s "$relative_target" "$temp_link" \
        || refuse "could not create temporary CLI face link for ck-$name"
      if ! mv -f "$temp_link" "$face_path"; then
        rm -f "$temp_link"
        refuse "could not publish CLI face link ck-$name"
      fi
      say "cli face: created ck-$name -> $DEST"
    done
  fi
  if [ "${#FACE_REMOVE_PATHS[@]}" -gt 0 ]; then
    for link in "${FACE_REMOVE_PATHS[@]}"; do
      # Re-read immediately before unlinking so a user-replaced link survives.
      if face_link_resolves_to_dest "$link"; then
        rm "$link" || refuse "could not remove stale CLI face ${link##*/}"
        say "cli face: removed ${link##*/} -> $DEST"
      else
        refuse "stale CLI face ${link##*/} no longer points to $DEST; left it unchanged"
      fi
    done
  fi
}

# Probe an image's manifest once, validate its CLI face declaration, and expose
# the raw JSON for the provenance check that normal placement also performs.
probe_cli_faces() {
  local image="$1" probe_home face_probe_result
  FACE_NAMES=()
  FACE_STATE="not_checked"
  FACE_PROBE_JSON=""
  probe_home=$(mktemp -d)
  FACE_PROBE_JSON=$(XDG_DATA_HOME="$probe_home/data" XDG_RUNTIME_DIR="$probe_home/run" XDG_CONFIG_HOME="$probe_home/config" \
    perl -e 'alarm 5; exec @ARGV' "$image" --manifest 2>/dev/null </dev/null) || FACE_PROBE_JSON=""
  rm -rf "$probe_home"
  face_probe_result=$(printf '%s' "$FACE_PROBE_JSON" | python3 -c '
import json, os, re, sys

class DuplicateKeyError(ValueError):
    pass

def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateKeyError(f"duplicate JSON object key {key}")
        result[key] = value
    return result

raw = sys.stdin.read()
try:
    manifest = json.loads(raw, object_pairs_hook=unique_object)
except DuplicateKeyError as error:
    print(f"invalid: {error}")
    raise SystemExit(0)
except Exception:
    print("not_checked")
    raise SystemExit(0)
if not isinstance(manifest, dict):
    print("not_checked")
    raise SystemExit(0)
if "cli_faces" not in manifest:
    print("absent")
    raise SystemExit(0)
faces = manifest["cli_faces"]
if not isinstance(faces, list):
    print("invalid: cli_faces must be an array")
    raise SystemExit(0)
seen = set()
for index, face in enumerate(faces):
    if not isinstance(face, dict) or set(face) != {"name", "binary"}:
        print(f"invalid: cli_faces[{index}] must contain exactly name and binary")
        raise SystemExit(0)
    name, binary = face["name"], face["binary"]
    if not isinstance(name, str) or name.startswith("ck-") or not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name, re.ASCII):
        print(f"invalid: cli_faces[{index}].name is not a lowercase command name")
        raise SystemExit(0)
    if name in seen:
        print(f"invalid: duplicate cli face name {name}")
        raise SystemExit(0)
    seen.add(name)
    if not isinstance(binary, str) or binary != os.path.basename(sys.argv[1]):
        print(f"invalid: cli_faces[{index}].binary must equal {os.path.basename(sys.argv[1])}")
        raise SystemExit(0)
print("present")
for name in sorted(seen):
    print(name)
' "$DEST") || face_probe_result="not_checked"
  case "$face_probe_result" in
    not_checked) FACE_STATE="not_checked" ;;
    absent) FACE_STATE="absent" ;;
    invalid:*) refuse "invalid cli_faces in --manifest: ${face_probe_result#invalid: }" ;;
    present|$'present\n'*)
      FACE_STATE="present"
      face_names_text="${face_probe_result#present}"
      while IFS= read -r face_name; do
        [ -n "$face_name" ] && FACE_NAMES+=("$face_name")
      done <<< "${face_names_text#$'\n'}"
      ;;
    *) FACE_STATE="not_checked" ;;
  esac
  return 0
}

rollback_mode() {
  local rollback_file rollback_sidecar rollback_digest sidecar_digest current_backup backup_tmp
  local current_digest backup_digest store_snapshot store_saved_dir suffix restore_suffix source_path saved_path
  local integrity_output integrity_status placed_digest placed_version restart_out
  rollback_file="$STAGING/$(basename "$DEST").rollback-$ROLLBACK"
  rollback_sidecar="$rollback_file.sha256"
  [ -f "$rollback_file" ] || refuse "rollback snapshot not found: $rollback_file"
  [ -f "$rollback_sidecar" ] || refuse "rollback sidecar not found: $rollback_sidecar"
  (cd "$STAGING" && shasum -c "$(basename "$rollback_sidecar")" >/dev/null 2>&1) \
    || refuse "rollback sidecar does not verify its artifact: $(basename "$rollback_sidecar")"
  rollback_digest=$(shasum -a 256 "$rollback_file" | awk '{print $1}')
  sidecar_digest=$(awk 'NR==1{print $1}' "$rollback_sidecar")
  [ -n "$sidecar_digest" ] && [ "$rollback_digest" = "$sidecar_digest" ] \
    || refuse "rollback sidecar digest does not describe its named snapshot"

  store_snapshot=""
  if [ -n "$RESTORE_STORE" ]; then
    store_snapshot="$STAGING/$MODULE.store.db.rollback-$ROLLBACK"
    [ -f "$store_snapshot" ] || refuse "store rollback snapshot not found: $store_snapshot"
  else
    say "store: untouched (--restore-store not given)"
  fi

  # A retained binary must still satisfy every format floor recorded by its
  # module; restoring an older image does not make newer on-disk data readable.
  floor_check="$(dirname "$0")/check-format-floors.sh"
  "$floor_check" "$MODULE" "$rollback_file" || exit 2
  probe_cli_faces "$rollback_file"
  prepare_cli_face_plan

  if [ "$PLACE" != "1" ]; then
    print_cli_face_plan
    say "rollback: would restore $rollback_file (sha ${rollback_digest:0:8})"
    [ -z "$RESTORE_STORE" ] || say "store: would restore $store_snapshot to $RESTORE_STORE"
    say "=== rollback checked, NOTHING placed and NOTHING restarted (pass --place to mutate)"
    return 0
  fi

  mkdir -p "$STAGING"
  current_backup="$STAGING/$(basename "$DEST").rollback-$(date -u +%Y%m%dT%H%M%SZ)"
  [ ! -e "$current_backup" ] || refuse "a rollback snapshot already exists for this UTC second: $current_backup"
  backup_tmp="$current_backup.tmp.$$"
  cp "$DEST" "$backup_tmp" || { rm -f "$backup_tmp"; refuse "could not save the current binary before rollback"; }
  current_digest=$(shasum -a 256 "$DEST" | awk '{print $1}')
  backup_digest=$(shasum -a 256 "$backup_tmp" | awk '{print $1}')
  if [ -z "$current_digest" ] || [ "$current_digest" != "$backup_digest" ]; then
    rm -f "$backup_tmp"
    refuse "current binary rollback copy differs from its source; nothing has been placed"
  fi
  mv "$backup_tmp" "$current_backup" || refuse "could not publish the current binary rollback copy"
  (cd "$STAGING" && shasum -a 256 "$(basename "$current_backup")" > "$(basename "$current_backup").sha256") \
    || refuse "could not write the current binary rollback sidecar"
  if [ -f "$DEST.format-versions.json" ]; then
    cp "$DEST.format-versions.json" "$current_backup.format-versions.json"
  fi
  say "rollback $(basename "$current_backup") matches live (${current_digest:0:8})"

  if [ -n "$RESTORE_STORE" ]; then
    local stop_out
    if ! stop_out=$(ck module stop "$MODULE" 2>&1); then
      refuse "could not stop '$MODULE' before restoring its store: $stop_out"
    fi
    [ -z "$stop_out" ] || printf '%s\n' "$stop_out" | tail -1
    store_saved_dir=$(mktemp -d "$STAGING/.${MODULE}.store-set-aside.XXXXXX") \
      || refuse "could not create an owner-only store preservation folder in $STAGING"
    chmod 700 "$store_saved_dir"
    for suffix in "" -wal -shm; do
      source_path="$RESTORE_STORE$suffix"
      if [ -e "$source_path" ] || [ -L "$source_path" ]; then
        if ! mv "$source_path" "$store_saved_dir/$(basename "$RESTORE_STORE")$suffix"; then
          for restore_suffix in "" -wal -shm; do
            saved_path="$store_saved_dir/$(basename "$RESTORE_STORE")$restore_suffix"
            [ -e "$saved_path" ] || [ -L "$saved_path" ] || continue
            mv "$saved_path" "$RESTORE_STORE$restore_suffix" || refuse "could not put the set-aside store files back; preserved copies remain in $store_saved_dir"
          done
          refuse "could not move the current store files into $store_saved_dir"
        fi
      fi
    done
    if ! cp "$store_snapshot" "$RESTORE_STORE" || ! chmod 600 "$RESTORE_STORE"; then
      rm -f "$RESTORE_STORE"
      for suffix in "" -wal -shm; do
        saved_path="$store_saved_dir/$(basename "$RESTORE_STORE")$suffix"
        [ -e "$saved_path" ] || [ -L "$saved_path" ] || continue
        mv "$saved_path" "$RESTORE_STORE$suffix" || refuse "could not restore the set-aside store files; preserved copies remain in $store_saved_dir"
      done
      refuse "could not copy the store snapshot into place with mode 0600"
    fi
    integrity_status=0
    integrity_output=$(sqlite3 "$RESTORE_STORE" 'PRAGMA integrity_check' 2>&1) || integrity_status=$?
    if [ "$integrity_status" -ne 0 ] || [ "$integrity_output" != "ok" ]; then
      rm -f "$RESTORE_STORE"
      for suffix in "" -wal -shm; do
        saved_path="$store_saved_dir/$(basename "$RESTORE_STORE")$suffix"
        [ -e "$saved_path" ] || [ -L "$saved_path" ] || continue
        mv "$saved_path" "$RESTORE_STORE$suffix" || refuse "restored store failed integrity_check and the prior files could not be restored; preserved copies remain in $store_saved_dir"
      done
      refuse "restored store failed PRAGMA integrity_check (${integrity_output:-no result}); prior store files were restored and the module was not started"
    fi
    say "store integrity_check: ok; prior store files preserved in $store_saved_dir"
  fi

  say "=== place"
  cp "$rollback_file" "$DEST.tmp" && mv "$DEST.tmp" "$DEST"
  if [ -f "$rollback_file.format-versions.json" ]; then
    cp "$rollback_file.format-versions.json" "$DEST.format-versions.json.tmp" \
      && mv "$DEST.format-versions.json.tmp" "$DEST.format-versions.json"
  else
    rm -f "$DEST.format-versions.json"
  fi
  placed_digest=$(shasum -a 256 "$DEST" | awk '{print $1}')
  [ -n "$placed_digest" ] && [ "$placed_digest" = "$rollback_digest" ] \
    || refuse "placed bytes differ from the verified rollback snapshot (snapshot $rollback_digest, placed $placed_digest)"
  say "placed sha ${placed_digest:0:8} (equals rollback snapshot)"
  placed_version=$("$DEST" --version 2>&1 | head -1)
  say "warm-exec at destination: $placed_version"
  reconcile_cli_faces

  prune="$(dirname "$0")/prune-rollbacks.sh"
  if ! CK_STAGING="$STAGING" "$prune" --keep 3 --apply "$(basename "$DEST")"; then
    say "WARNING: rollback prune failed; the rollback itself succeeded and older snapshots were kept"
  fi
  say "rolled back to $ROLLBACK"

  if [ "$RESTART" -eq 1 ]; then
    say "=== restart"
    if [ -n "$RESTORE_STORE" ]; then
      if ! restart_out=$(ck module start "$MODULE" 2>&1); then
        refuse "start of '$MODULE' failed after rollback; the restored binary and store are on disk: ${restart_out}"
      fi
    else
      if ! restart_out=$(ck module restart "$MODULE" 2>&1); then
        refuse "restart of '$MODULE' failed after rollback; the restored binary is on disk: ${restart_out}"
      fi
    fi
    printf '%s\n' "$restart_out" | tail -1
    say "$(date -u +%FT%TZ) restart initiated -- verify on a lane opened AFTER this point:"
    say "  ck --json provenance $MODULE | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"modules\"][0][\"daemon_observed\"][\"pid\"])'"
    say "  then inode proc-vs-disk on that pid, which is the only proof it runs these bytes"
    say "  logs: event in the module's r2 sink ~/.local/share/cortexkit/$MODULE/logs/$MODULE.<YYYY-MM-DD>.log (timestamped)"
    say "        archive ~/.local/share/cortexkit/run/logs/$MODULE.stderr.log must not GROW ($(wc -c < ~/.local/share/cortexkit/run/logs/$MODULE.stderr.log 2>/dev/null || echo 0) bytes now); a line found there is historical"
  else
    [ -z "$RESTORE_STORE" ] || say "module remains stopped (--no-restart)"
    say "restart skipped (--no-restart)"
  fi
}

if [ -n "$ROLLBACK" ]; then
  rollback_mode
  exit 0
fi

# BEFORE-READINGS: what exists only while the OUTGOING image runs.
#
# ENGRAM's rule (2026-09-19), from a quota incident where they killed a runaway
# loop and lost the one reading that would have said whether it was about to
# stop itself: BEFORE AN INTERVENTION THAT ENDS A LIVE PHENOMENON, ASK WHAT
# READING ONLY EXISTS WHILE IT IS RUNNING, AND TAKE THAT READING FIRST.
#
# A placement IS such an intervention -- the outgoing process dies at the
# restart, taking its gauges, counters and in-flight state with it. Every other
# arm here reads the FILE, so none of them can supply this. It was being done ad
# hoc when a card happened to ask, which is how I placed an aft card and only
# afterwards wanted the outgoing image's steady-state write rate, by which point
# the process carrying it was gone.
#
# Output is labelled and printed; it decides NOTHING, because a reading whose
# meaning is known in advance belongs in --marker or --control instead.
if [ -n "${BEFORE_CMD:-}" ]; then
  say "=== before-readings (from the OUTGOING image; gone after restart)"
  sh -c "$BEFORE_CMD" 2>&1 | sed 's/^/  /' \
    || say "  (before-reading exited non-zero; recorded, not fatal)"
fi

say "=== gate"

# Staged sidecar, verified the way a consumer verifies it.
staged_dir=$(cd "$(dirname "$STAGED")" && pwd); staged_base=$(basename "$STAGED")
sidecar=""
for cand in "$staged_base.sha256.postsign" "$staged_base.sha256"; do
  [ -f "$staged_dir/$cand" ] && { sidecar="$cand"; break; }
done
[ -n "$sidecar" ] || refuse "no sidecar beside the staged artifact (bare-binary staging directories are refused)"
(cd "$staged_dir" && shasum -c "$sidecar" >/dev/null 2>&1) || refuse "staged sidecar does not verify its own artifact: $sidecar"

# BOTH FILES MUST BE THE SAME KIND OF THING, and this is a separate question
# from every other arm here.
#
# `strings` and `nm` produce output for files that are not the binary you meant:
# a shell script, a text file, an archive. So a marker reading "staged 1 / live
# 0" and a control reading 1/1 can BOTH be satisfied by a text file that happens
# to contain those words -- every arm green, nothing placed that runs.
#
# PLEX found this by adversarial test of their own staging script (2026-09-19):
# it PASSED against a plain text file containing the control word. Their line is
# the general one -- A PRESENT CONTROL PROVES THE INSTRUMENT READ SOMETHING, NOT
# THAT IT READ THE RIGHT KIND OF THING.
#
# My gate happened to refuse their exact case, but on SIGNING POSTURE ("staged []
# vs running [Signature=adhoc]") -- an accident of this host, not a design. With
# codesign absent, or a running binary that is itself unsigned, the text file
# sails through.
staged_kind=$(file -b "$STAGED" 2>/dev/null | cut -d, -f1)
kind_ref="$DEST"
# A first install has no running binary, so the daemon's own binary stands in:
# a module must be the same kind of executable the host already runs.
[ "$INSTALL" -eq 1 ] && kind_ref="$BIN_DIR/ck-subc"
live_kind=$(file -b "$kind_ref" 2>/dev/null | cut -d, -f1)
case "$staged_kind" in
  *"Mach-O"*|*"ELF"*|*"PE32"*) : ;;
  *) refuse "staged artifact is not an executable image: $STAGED reads as \"$staged_kind\". strings and nm answer for text files too, so the marker and control arms below would be satisfied by a file that cannot run; nothing has been placed" ;;
esac
[ "$staged_kind" = "$live_kind" ] || refuse "staged and running artifacts are DIFFERENT KINDS: staged \"$staged_kind\" vs running \"$live_kind\"; nothing has been placed"
say "kind: $staged_kind (matches $( [ "$INSTALL" -eq 1 ] && echo "the daemon's binary" || echo running))"
say "staged sidecar $sidecar: OK"

# ---- CURRENCY: is this the artifact its OWNER says is live? ----
#
# TWO MECHANISMS, AND ONE OF THEM IS AUTHORITATIVE.
#
#   ck-<module>.current   a manifest the module's owner writes when they hand
#                         over a card: "<sha256>  <filename>  <UTC stamp>".
#                         It STATES a fact. Authoritative.
#   newest-by-mtime       this gate INFERS from file ordering. A fallback, and
#                         a poor one -- the directory accumulates leftovers, so
#                         the newest file may be stale too.
#
# They agreed the first night the manifest existed (broca 0.3.98). THAT
# AGREEMENT IS EXACTLY WHEN TO DECIDE WHICH ONE WINS, rather than treating
# concord as validation and discovering the ordering at the moment they
# disagree -- which would be a placement, with an operator waiting.
#
# So: manifest present -> it decides, and the mtime arm is not consulted at all.
# Manifest absent -> mtime, and the line SAYS it is inferring, because a
# fallback that reads like a verdict is how a guess becomes a fact.
#
# Measured 2026-09-18, which is why any of this exists: 84 staged binaries across
# all modules, 26 for broca alone with verifying sidecars, every one passing this
# gate's only placeability test. A broca card was superseded upstream hours after
# it was staged and gated, and I learned it from its owner rather than from here.
# TWO DECLARATION SHAPES, AND THE ROOT-LEVEL ONE IS STRICTLY STRONGER.
#
#   <stage>/ck-<module>.current   in-stage. "<sha256>  <file>  <stamp>".
#   <staging root>/<module>.current   beside the stages, key=value, carrying
#                                     stage= and revision=.
#
# An IN-STAGE manifest can only say "this stage is current", because to read it
# I must already have chosen a directory -- so it is a SELF-ATTESTATION, and
# every stage can carry one saying yes. It catches the case where I pass a stale
# directory that has no manifest, and misses the case where the stale directory
# has one. The ROOT-LEVEL form NAMES the current stage, which is the question the
# mtime fallback was guessing at.
#
# FUSI built the root form (2026-09-19) after my gate printed "INFERRED from
# mtime" on one of their cards; PLEX and three others already write the in-stage
# form. Both are read, root first, because a seat that improved its declaration
# must not be punished for it and a seat that has not must keep working.
# TWO STAGING LAYOUTS EXIST AND NEITHER IS WRONG, so the declaration is looked
# for in BOTH plausible places and parsed by CONTENT rather than by location.
#
#   PER-STAGE DIRECTORY (fusiform, broca)   ~/ck-stage/fusiform-<stamp>/ck-fusiform
#       -> the root is the PARENT of the artifact's directory
#   FLAT, SHA-SUFFIXED FILES (plexus)       <staging>/SIGNED.ck-plexus.<sha>
#       -> the artifact's directory IS the root
#
# Deriving only "parent of the artifact directory" looked one level too high for
# the flat layout and found nothing. Measured against plexus 2026-09-19: the
# lookup landed on ~/.local/share/cortexkit/plexus.current, which does not exist,
# so the gate fell through to the in-stage arm and refused a CORRECT artifact
# with a message naming the wrong cause.
#
# FORMAT IS DECIDED BY CONTENT, NOT BY PATH. A file containing `stage=` is the
# key=value form; anything else is the older positional `<sha> <file> <stamp>`.
# Deciding by location is how a seat that adopts the better format at the old
# filename gets its correct declaration parsed as garbage -- which is exactly
# what happened when PLEX rewrote ck-plexus.current in the new shape.
# The currency pointer names the BINARY being placed, not the module id. A
# module can ship more than one binary (plexus ships ck-plexus and
# ck-plexus-admin), each with its own pointer; keying the lookup on --module
# made the admin placement read the daemon's pointer and refuse a correct
# artifact. CUR is the destination binary's name without its "ck-" prefix, so
# for the usual single-binary module it equals --module and nothing changes.
CUR=$(basename "$DEST"); CUR=${CUR#ck-}
# Every plausible spelling is read, not just the first one found, because two
# of them can exist at once (the owner writes ck-<name>.current while an older
# <name>.current lingers). Reading only the first let a stale declaration decide
# which stage is live. If two declarations name different stages or revisions,
# the owner's statement is ambiguous and the gate refuses until it is resolved.
root_manifest=""
for cand in "$(dirname "$staged_dir")/$CUR.current" \
            "$staged_dir/$CUR.current" \
            "$(dirname "$staged_dir")/ck-$CUR.current" \
            "$staged_dir/ck-$CUR.current"; do
  [ -f "$cand" ] && grep -q '^stage=' "$cand" 2>/dev/null || continue
  if [ -z "$root_manifest" ]; then
    root_manifest="$cand"
    continue
  fi
  first_decl="$(awk -F= '$1=="stage"||$1=="revision"{print $2}' "$root_manifest")"
  this_decl="$(awk -F= '$1=="stage"||$1=="revision"{print $2}' "$cand")"
  if [ "$first_decl" != "$this_decl" ]; then
    echo "REFUSED: two currency declarations for $CUR disagree" >&2
    echo "         $root_manifest: $(echo $first_decl)" >&2
    echo "         $cand: $(echo $this_decl)" >&2
    echo "         Remove the stale one (record its content first), then retry." >&2
    exit 2
  fi
done
# A currency declaration is the module owner's statement of which staged build
# is live. It comes in two shapes: key=value ("stage=<path>", checked above)
# and the older positional "<sha256>  <file>  <stamp>". The check above reads
# only the key=value shape, so a leftover positional file naming an older
# build could sit beside a correct key=value one and never be read. Here every
# declaration, in either shape, either names the artifact being placed or
# names something else. If some do and some don't, the owner's statement is
# ambiguous, and the gate refuses even with --older, since --older means
# "place an older build on purpose", not "ignore a contradiction".
have_sha_all=$(shasum -a256 "$STAGED" | awk '{print $1}')
yes_decls=""; no_decls=""
for cand in "$(dirname "$staged_dir")/$CUR.current" \
            "$staged_dir/$CUR.current" \
            "$(dirname "$staged_dir")/ck-$CUR.current" \
            "$staged_dir/ck-$CUR.current"; do
  [ -f "$cand" ] || continue
  if grep -q '^stage=' "$cand" 2>/dev/null; then
    decl_stage=$(awk -F= '$1=="stage"{print $2}' "$cand")
    case "$decl_stage" in
      /*) ;;
      *) decl_stage="$(cd "$(dirname "$cand")" && pwd)/$decl_stage" ;;
    esac
    if [ "$staged_dir" = "$decl_stage" ] || [ "$STAGED" = "$decl_stage" ]; then
      yes_decls="$yes_decls $cand"
    else
      no_decls="$no_decls $cand"
    fi
  else
    if [ "$(awk 'NR==1{print $1}' "$cand")" = "$have_sha_all" ]; then
      yes_decls="$yes_decls $cand"
    else
      no_decls="$no_decls $cand"
    fi
  fi
done
if [ -n "$yes_decls" ] && [ -n "$no_decls" ]; then
  echo "REFUSED: currency declarations for $CUR disagree about which artifact is live" >&2
  echo "         name $(basename "$STAGED"):$yes_decls" >&2
  echo "         name something else:$no_decls" >&2
  echo "         Remove the stale one (record its content first), then retry." >&2
  exit 2
fi
manifest="$staged_dir/ck-$CUR.current"
staged_base=$(basename "$STAGED")
if [ -n "$root_manifest" ]; then
  want_stage=$(awk -F= '$1=="stage"{print $2}' "$root_manifest")
  want_rev=$(awk -F= '$1=="revision"{print $2}' "$root_manifest")
  # A relative `stage=` (a bare stage name, as some seats write it) names a path
  # beside the manifest. Resolve it there; comparing it literally against an
  # absolute path refused a correct card.
  case "$want_stage" in
    /*) ;;
    *) want_stage="$(cd "$(dirname "$root_manifest")" && pwd)/$want_stage" ;;
  esac
  # `stage` names a DIRECTORY under the per-stage layout and the ARTIFACT itself
  # under the flat one. Accept either rather than forcing a seat to restructure:
  # what the declaration is FOR is naming which thing is live, and both spellings
  # do that unambiguously.
  if [ "$staged_dir" = "$want_stage" ] || [ "$STAGED" = "$want_stage" ]; then
    say "currency: matches $CUR.current (owner-declared stage), rev ${want_rev:0:12}"
  elif [ "$OLDER" -eq 1 ]; then
    say "currency: placing from $staged_dir although the owner declares $want_stage (--older given)"
  else
    echo "REFUSED: $staged_dir is not the stage $CUR.current declares" >&2
    echo "         owner declares: $want_stage" >&2
    echo "         you passed:     $staged_dir" >&2
    echo "         The root manifest is the owner's statement of WHICH STAGE is live," >&2
    echo "         which an in-stage manifest cannot answer. If this is deliberate" >&2
    echo "         (a rollback), pass --older." >&2
    exit 2
  fi
elif [ -f "$manifest" ]; then
  want_sha=$(awk 'NR==1{print $1}' "$manifest")
  want_file=$(awk 'NR==1{print $2}' "$manifest")
  have_sha=$(shasum -a256 "$STAGED" | awk '{print $1}')
  if [ "$have_sha" = "$want_sha" ]; then
    # Name matching is secondary: the sha is the identity. A renamed copy of the
    # current bytes is the current artifact.
    say "currency: matches ck-$CUR.current (owner-declared), sha ${have_sha:0:16}"
  elif [ "$OLDER" -eq 1 ]; then
    say "currency: placing $staged_base although the owner declares $want_file current (--older given)"
  else
    echo "REFUSED: $staged_base is not what ck-$CUR.current declares" >&2
    echo "         owner declares: $want_file  ${want_sha:0:16}" >&2
    echo "         you passed:     $staged_base  ${have_sha:0:16}" >&2
    echo "         The manifest is the module owner's statement of which artifact is" >&2
    echo "         live. If this is deliberate (a rollback), pass --older." >&2
    exit 2
  fi
else
  # `|| true` IS LOAD-BEARING AND WAS MISSING. Under `set -euo pipefail`, a grep
  # that matches nothing exits 1, pipefail propagates it to the assignment, and
  # set -e KILLS THE SCRIPT -- after the sidecar line and before any other arm.
  # The operator sees one line of output and a script that stopped, which reads
  # like a gate that finished rather than one that died.
  #
  # It fires whenever a staging directory holds no `ck-<module>.<hex>` artifact
  # -- which is every FUSI card, because they name theirs plainly `ck-fusiform`
  # inside a timestamped directory. A NAMING CONVENTION THIS GATE INVENTED,
  # silently refusing every artifact that does not follow it.
  #
  # Found 2026-09-19 by running the gate on a card and getting two lines back,
  # then `bash -x` rather than assuming the run was fine. My own check reported
  # `exit=0` because I read `$?` through a pipe and got `tail`'s status -- the
  # exit-code trap from the same evening, inside the verification of the tool
  # that catches it.
  newest=$(ls -t "$staged_dir" 2>/dev/null \
    | grep -E "^(SIGNED\.)?ck-$MODULE\.[0-9a-f]+$" \
    | head -1) || true
  if [ -n "$newest" ] && [ "$newest" != "$staged_base" ]; then
    if [ "$OLDER" -eq 1 ]; then
      say "currency: INFERRED from mtime (no ck-$CUR.current); placing $staged_base although $newest is newer (--older given)"
    else
      echo "REFUSED: $staged_base is not the newest staged artifact for $MODULE" >&2
      echo "         newer: $newest" >&2
      echo "         INFERRED FROM MTIME -- there is no ck-$CUR.current manifest, so" >&2
      echo "         this gate is GUESSING from file ordering. The named file may be" >&2
      echo "         stale too; it is a fact about timestamps, not a recommendation." >&2
      echo "         Ask the module owner to write ck-$CUR.current, or pass --older." >&2
      exit 2
    fi
  else
    say "currency: INFERRED from mtime (no ck-$CUR.current); $staged_base is newest"
  fi
fi

# The sidecar and the currency declaration prove the bytes are the staged file
# the owner named; they do not prove those bytes were built from the commit the
# owner named. A module's `--manifest` reports its build provenance: the commit
# it was built from, or why it stamped none (a dirty tree declines to stamp).
# Entorhinal 0.1.22 and 0.1.23 were both placed from dirty trees, attesting no
# commit, and nothing here noticed. So: refuse an unstamped build, and refuse a
# stamped commit that disagrees with the declared revision, unless
# --allow-unstamped is given on purpose.
#
# The probe runs the staged binary with only `--manifest`, under a throwaway
# XDG home and a 5 s alarm, so a binary that does not know the flag cannot touch
# real stores or hang the gate. A binary that does not answer is reported as
# unchecked rather than refused: not every module serves `--manifest` yet.
probe_cli_faces "$STAGED"
prov_json="$FACE_PROBE_JSON"
prov_line=$(printf '%s' "$prov_json" | python3 -c '
import json, sys
try:
    p = json.load(sys.stdin).get("provenance") or {}
except Exception:
    print("unreported"); sys.exit(0)
sha = p.get("build_git_sha") or ""
why = p.get("build_git_sha_absence_reason") or ""
print("stamped " + sha if sha else ("unstamped " + why if why else "unreported"))
' 2>/dev/null || echo unreported)
case "$prov_line" in
  "stamped "*)
    stamped=${prov_line#stamped }
    if [ -n "${want_rev:-}" ]; then
      n=${#stamped}; [ ${#want_rev} -lt "$n" ] && n=${#want_rev}
      if [ "${stamped:0:$n}" = "${want_rev:0:$n}" ]; then
        say "provenance: built from ${stamped:0:12}, matching the declared revision"
      elif [ "$ALLOW_UNSTAMPED" -eq 1 ]; then
        say "provenance: built from ${stamped:0:12}, NOT the declared ${want_rev:0:12} (--allow-unstamped given)"
      else
        refuse "staged binary was built from ${stamped:0:12}, but the currency declaration names revision ${want_rev:0:12}. Rebuild from the declared commit, fix the declaration, or pass --allow-unstamped on purpose"
      fi
    else
      say "provenance: built from ${stamped:0:12}; no declared revision to compare (the declaration has no revision=)"
    fi ;;
  "unstamped "*)
    why=${prov_line#unstamped }
    if [ "$ALLOW_UNSTAMPED" -eq 1 ]; then
      say "provenance: build stamps no commit ($why); placed unstamped by request (--allow-unstamped)"
    else
      refuse "staged binary stamps no commit ($why), so it cannot prove which source it was built from. Rebuild from a clean tree, or pass --allow-unstamped on purpose"
    fi ;;
  *)
    say "provenance: not checked (the binary did not answer --manifest)" ;;
esac

# Signing posture must match the running image: an ad-hoc re-sign of a Developer ID
# binary silently revokes its macOS TCC grants, and the reverse is a surprise too.
# The posture is every field a grant or the loader keys on, not just the signer:
# the identifier is part of the designated requirement TCC grants are bound to,
# so a changed identifier revokes them exactly as an ad-hoc re-sign does; the
# team is the signer's scope; and the CodeDirectory flags carry the hardened
# runtime, which changes what the loader permits. Comparing only the first
# Signature/Authority line passed a re-sign under a different identifier.
signing_posture() {
  codesign -dvv "$1" 2>&1 \
    | grep -E '^(Signature|Identifier|TeamIdentifier)=|^Authority=|^CodeDirectory ' \
    | sed -E 's/^CodeDirectory .*(flags=[^ ]+).*/\1/' \
    | grep -vE '^Authority=Apple (Worldwide|Root)' \
    | sort | tr '\n' ' ' || true
}
# The hardened-runtime flag (CodeDirectory 0x10000) is the one signing-posture
# change a placement may make, and only from off to on. Each module holds a launch
# nonce that admits any connection presenting it as that module; hardened runtime
# is what stops another process of the same user attaching a debugger and reading
# it (docs/launch-nonce.md). Modules gain the flag one placement
# at a time, so an exact match on it would refuse every module's first hardened
# build. Removing it is refused, because that lets same-user processes attach again.
cd_flags() {
  codesign -dvv "$1" 2>&1 | sed -n -E 's/^CodeDirectory .*flags=(0x[0-9a-fA-F]+).*/\1/p' | head -1
}
has_runtime() {
  local f; f=$(cd_flags "$1")
  [ -n "$f" ] && [ $(( f & 0x10000 )) -ne 0 ]
}
# The posture with the flags reduced to everything except the runtime bit, so the
# exact-match rule still covers ad-hoc-ness and every other flag.
posture_without_runtime() {
  local f bare
  f=$(cd_flags "$1")
  bare=$(signing_posture "$1" | sed -E 's/flags=[^ ]+ //')
  if [ -n "$f" ]; then printf '%sflags-sans-runtime=0x%x ' "$bare" $(( f & ~0x10000 )); else printf '%s' "$bare"; fi
}
# A LINKER-SIGNED running image (flag 0x20000) was signed by `ld` at link time, not
# by codesign. Its identifier is a per-build hash (`ck_astrocyte-<16 hex>`), so it
# differs between any two builds, and any codesign re-sign clears the 0x20000 bit,
# which adding hardened runtime requires. Its designated requirement is a cdhash,
# so no privacy grant can survive a rebuild of it anyway. Comparing those two
# fields refused every first hardened build of a linker-signed module. When the
# running image is linker-signed, drop the identifier and the linker-signed bit
# from both sides; ad-hoc-ness, team and every other flag still have to match.
is_linker_signed() {
  local f; f=$(cd_flags "$1")
  [ -n "$f" ] && [ $(( f & 0x20000 )) -ne 0 ]
}
# An ad-hoc image that a bare `codesign --force --sign -` re-signed after linking
# has lost the linker-signed bit but keeps the linker's per-build identifier
# (`<name>-<hex hash>`), which no later build can reproduce. Its designated
# requirement is a cdhash, so the identifier carries no grant. Treat it like a
# linker-signed image for the identifier only; its flags are compared normally.
has_linker_identifier() {
  local id
  id=$(codesign -dvv "$1" 2>&1 | sed -n 's/^Identifier=//p' | head -1)
  [ "$(codesign -dvv "$1" 2>&1 | grep -c '^Signature=adhoc')" -gt 0 ] \
    && printf '%s' "$id" | grep -qE -- '-[0-9a-f]{16,}$'
}
posture_for_linker_signed() {
  local f bare
  f=$(cd_flags "$1")
  bare=$(signing_posture "$1" | sed -E 's/flags=[^ ]+ //; s/Identifier=[^ ]+ //')
  if [ -n "$f" ]; then printf '%sflags-sans-runtime-linker=0x%x ' "$bare" $(( f & ~0x30000 )); else printf '%s' "$bare"; fi
}
# Debug-map entries (`nm -a` type "-") are what `strip` removes; a release card
# has none. The one exception is the linker's `OPT radr://5614542` marker, which
# `strip` deliberately keeps and which carries no debug information. The count is
# read to a variable rather than branched on through a pipeline, so the check
# cannot invert under pipefail.
if command -v nm >/dev/null; then
  debug_entries=$(nm -a "$STAGED" 2>/dev/null | awk '$2 == "-" && $5 != "OPT"' | wc -l | tr -d ' ')
  if [ "${debug_entries:-0}" -gt 0 ] && [ "$ALLOW_UNSTRIPPED" -eq 1 ]; then
    say "debug map: $debug_entries entries, placed unstripped by request (--allow-unstripped)"
  elif [ "${debug_entries:-0}" -gt 0 ]; then
    refuse "staged binary is not stripped: $debug_entries debug-map entries (nm -a type '-'). Build the card stripped, or pass --allow-unstripped to place it on purpose"
  else
    say "debug map: 0 entries (stripped)"
  fi
fi
if [ "$INSTALL" -eq 1 ] && command -v codesign >/dev/null; then
  # No running binary to match, so the posture is checked against fleet rules.
  staged_sig=$(signing_posture "$STAGED")
  staged_cd=$(codesign -dvv "$STAGED" 2>&1 || true)
  staged_id=$(printf '%s\n' "$staged_cd" | sed -n 's/^Identifier=//p' | head -1)
  [ "$staged_id" = "$(basename "$DEST")" ] \
    || refuse "staged identifier is [$staged_id], not the destination's name [$(basename "$DEST")]; sign with --identifier $(basename "$DEST")"
  has_runtime "$STAGED" || refuse "a first install must be hardened (codesign --options runtime): without it any same-user process can attach and read the module's launch nonce"
  ents=$(codesign -d --entitlements - "$STAGED" 2>/dev/null || true)
  if [ "$(printf '%s' "$ents" | grep -c 'get-task-allow')" -gt 0 ]; then
    refuse "staged binary carries com.apple.security.get-task-allow, which lets any same-user process attach and read its launch nonce"
  fi
  staged_team=$(printf '%s\n' "$staged_cd" | sed -n 's/^TeamIdentifier=//p' | head -1)
  if [ "$(printf '%s\n' "$staged_cd" | grep -c '^Signature=adhoc')" -gt 0 ]; then
    say "signing posture: $staged_sig(first install: ad-hoc, hardened)"
  else
    # Signed by an identity: it must be the team that signs the daemon itself,
    # so a binary signed by some other developer cannot enter the fleet.
    daemon_team=$(codesign -dvv "$BIN_DIR/ck-subc" 2>&1 | sed -n 's/^TeamIdentifier=//p' | head -1)
    [ -n "$daemon_team" ] && [ "$daemon_team" != "not set" ] && [ "$staged_team" = "$daemon_team" ] \
      || refuse "staged binary is signed by team [$staged_team], but a first install must be ad-hoc or signed by the daemon's own team [$daemon_team]"
    say "signing posture: $staged_sig(first install: the daemon's team $daemon_team, hardened)"
  fi
  exceptions=$(printf '%s' "$ents" | grep -oE 'com\.apple\.security\.cs\.[a-z-]+' | sort -u | paste -sd ' ' - || true)
  say "hardened runtime: yes; exceptions: ${exceptions:-none} (each needs the card's smoke test to exercise it)"
elif command -v codesign >/dev/null; then
  staged_sig=$(signing_posture "$STAGED")
  live_sig=$(signing_posture "$DEST")
  staged_sig_cmp=$(posture_without_runtime "$STAGED")
  live_sig_cmp=$(posture_without_runtime "$DEST")
  if is_linker_signed "$DEST"; then
    staged_sig_cmp=$(posture_for_linker_signed "$STAGED")
    live_sig_cmp=$(posture_for_linker_signed "$DEST")
    say "signing posture: running image is linker-signed; its per-build identifier and the linker-signed bit are not compared"
  elif has_linker_identifier "$DEST"; then
    staged_sig_cmp=$(printf '%s' "$staged_sig_cmp" | sed -E 's/Identifier=[^ ]+ //')
    live_sig_cmp=$(printf '%s' "$live_sig_cmp" | sed -E 's/Identifier=[^ ]+ //')
    say "signing posture: running image is ad-hoc with a linker per-build identifier; the identifier is not compared"
  fi
  if [ -n "$NEW_REQUIREMENT" ]; then
    # A requested requirement change may rename the identifier; every other
    # posture field (signer, team, ad-hoc-ness, flags) must still match.
    staged_sig_cmp=$(printf '%s' "$staged_sig_cmp" | sed -E 's/Identifier=[^ ]+ //')
    live_sig_cmp=$(printf '%s' "$live_sig_cmp" | sed -E 's/Identifier=[^ ]+ //')
  fi
  # The one signer change a placement may make: ad-hoc to the daemon's own team,
  # under the same identifier. An ad-hoc running image's designated requirement is
  # its cdhash, so no privacy grant is bound to it and none can be lost; a
  # team-signed image keeps grants across rebuilds, which is why a module that
  # needs one moves to the team. The reverse, and any other team, is refused.
  team_upgrade=0
  if [ "$(codesign -dvv "$DEST" 2>&1 | grep -c '^Signature=adhoc')" -gt 0 ] \
    && [ "$(codesign -dvv "$STAGED" 2>&1 | grep -c '^Signature=adhoc')" -eq 0 ]; then
    staged_team=$(codesign -dvv "$STAGED" 2>&1 | sed -n 's/^TeamIdentifier=//p' | head -1)
    daemon_team=$(codesign -dvv "$BIN_DIR/ck-subc" 2>&1 | sed -n 's/^TeamIdentifier=//p' | head -1)
    staged_ident=$(codesign -dvv "$STAGED" 2>&1 | sed -n 's/^Identifier=//p' | head -1)
    live_ident=$(codesign -dvv "$DEST" 2>&1 | sed -n 's/^Identifier=//p' | head -1)
    [ -n "$daemon_team" ] && [ "$daemon_team" != "not set" ] && [ "$staged_team" = "$daemon_team" ] \
      || refuse "staged binary moves from ad-hoc to team [$staged_team], but only the daemon's own team [$daemon_team] is accepted"
    if [ "$staged_ident" != "$live_ident" ] && ! { is_linker_signed "$DEST" || has_linker_identifier "$DEST"; }; then
      refuse "staged binary moves to the team under identifier [$staged_ident], but the running identifier is [$live_ident]; keep the identifier"
    fi
    has_runtime "$STAGED" || refuse "a move to the team must keep hardened runtime"
    team_upgrade=1
    say "signing posture: $staged_sig(ad-hoc to the daemon's team $daemon_team; no grant was bound to the ad-hoc image)"
  fi
  [ "$team_upgrade" -eq 1 ] || [ "$staged_sig_cmp" = "$live_sig_cmp" ] || refuse "signing posture differs: staged [$staged_sig] vs running [$live_sig]"
  if has_runtime "$DEST" && ! has_runtime "$STAGED" && [ "$ALLOW_UNHARDENED" -eq 1 ]; then
    say "hardened runtime: REMOVED BY REQUEST (--allow-unhardened): after this placement any same-user process can attach to this module and read its launch nonce, until a hardened build is placed again"
  elif has_runtime "$DEST" && ! has_runtime "$STAGED"; then
    refuse "hardened runtime would be REMOVED (pass --allow-unhardened to roll back to a pre-hardening build on purpose): the running binary has it and the staged one does not, which lets any same-user process attach and read the module's launch nonce (staged [$staged_sig] vs running [$live_sig])"
  fi
  if [ "$team_upgrade" -eq 1 ]; then
    :
  elif has_runtime "$STAGED" && ! has_runtime "$DEST"; then
    say "signing posture: $staged_sig(matches running except hardened runtime, which this placement ADDS)"
  else
    say "signing posture: $staged_sig(matches running)"
  fi
  # get-task-allow lets any same-user process attach whatever the runtime flag
  # says, so nothing in the fleet may ship it. The other hardened-runtime
  # exceptions (JIT, unsigned executable memory, library validation) do not reopen
  # attach, but a binary missing one it needs fails only when that code path runs
  # (a JIT that silently falls back, a Wasm guest killed on first execution). They
  # are printed so the placement card's smoke test can be checked to exercise each.
  ents=$(codesign -d --entitlements - "$STAGED" 2>/dev/null || true)
  if [ "$(printf '%s' "$ents" | grep -c 'get-task-allow')" -gt 0 ]; then
    refuse "staged binary carries com.apple.security.get-task-allow, which lets any same-user process attach and read its launch nonce"
  fi
  exceptions=$(printf '%s' "$ents" | grep -oE 'com\.apple\.security\.cs\.[a-z-]+' | sort -u | paste -sd ' ' - || true)
  if has_runtime "$STAGED"; then
    say "hardened runtime: yes; exceptions: ${exceptions:-none} (each needs the card's smoke test to exercise it)"
  else
    say "hardened runtime: NO (the launch-nonce boundary needs it before stage 7)"
  fi

  # TCC's own test, not an approximation of it: a macOS privacy grant is stored
  # against the designated requirement of the binary it was granted to, and a new
  # binary keeps the grant only if it SATISFIES that requirement. So check the
  # staged file against the running file's designated requirement with
  # `codesign -R`. The posture comparison above can pass where this fails (a
  # requirement that names a certificate field the posture does not print), and
  # when this fails the grant is gone with no prompt: ck-subc's identifier moved
  # from "ck-subc" to "SIGNED.ck-subc" (codesign derives it from the file name
  # when -i is omitted) and insula's Full Disk Access went dark, reading only as
  # "not permitted". An ad-hoc running binary's requirement is a cdhash, which
  # no rebuild can satisfy; that posture is already pinned above, so it is
  # reported and skipped here.
  live_dr=$(codesign -d -r- "$DEST" 2>&1 | sed -n 's/^designated => //p')
  staged_dr=$(codesign -d -r- "$STAGED" 2>&1 | sed -n 's/^designated => //p')
  if [ -n "$NEW_REQUIREMENT" ]; then
    # An INTENDED requirement change (a rename, or moving a grant from one
    # certificate to the team) revokes the running binary's privacy grants by
    # design, so it cannot pass the check below. The caller names the new
    # requirement exactly, and the staged binary must carry that requirement
    # and satisfy it. The grants still need re-granting once after placement,
    # which is why this is an explicit flag and never a default.
    [ "$staged_dr" = "$NEW_REQUIREMENT" ] \
      || refuse "--new-requirement names [$NEW_REQUIREMENT] but the staged binary's designated requirement is [$staged_dr]"
    codesign --verify -R="$NEW_REQUIREMENT" "$STAGED" >/dev/null 2>&1 \
      || refuse "staged binary does not satisfy its own named requirement [$NEW_REQUIREMENT]"
    say "designated requirement: CHANGING by request from [$live_dr] to [$NEW_REQUIREMENT]; macOS privacy grants held by the running binary (Full Disk Access, Accessibility) must be re-granted once after placement"
  elif [ -z "$live_dr" ]; then
    say "designated requirement: running binary has none (ad-hoc, cdhash); TCC grants cannot survive any rebuild of it"
  else
    dr_check=$(codesign --verify -R="$live_dr" "$STAGED" 2>&1) \
      || refuse "staged binary does not satisfy the running binary's designated requirement, so every macOS privacy grant (Full Disk Access, Accessibility) held by the running one would be silently revoked: [$live_dr] -- $dr_check"
    say "designated requirement: staged satisfies the running binary's ($live_dr)"
  fi

fi

# Marker differential. A marker that reads 0 on the staged file proves nothing about
# this build; a control that does not read on both proves the reader is broken.
#
# BOTH TABLES ARE READ, and the table is named in every line, because A MARKER COUNT
# IS SILENTLY READER-RELATIVE WITHOUT IT. A control-flow-only change adds no string
# literal, so `strings` cannot see it at all and reports a truthful 0 that means
# "wrong reader", not "wrong build" -- while a message literal is invisible to `nm`.
# Reading one table and reporting a bare count is how a valid placement gets refused
# and how an invalid one gets waved through; the pair plus the table name is decidable.
count_in() {  # count_in <table> <file> <needle>
  case "$1" in
    # `--` ends grep's options: a needle beginning with `-` (a CLI flag such
    # as `--ready-ms` is a natural marker) was otherwise parsed as an option,
    # grep printed nothing, and the gate refused a valid card as if the
    # marker were absent.
    nm) nm -a "$2" 2>/dev/null | grep -cF -- "$3" || true ;;
    *)  strings "$2" | grep -cF -- "$3" || true ;;
  esac
}
# A MARKER CAN DISCRIMINATE IN BOTH TABLES, AND THEN THE CONTROL PICKS WHICH ONE.
#
# This loop used to assign on every discriminating table, so the LAST one won --
# `nm` -- arbitrarily. A Rust string literal that is also a symbol name (a
# migration's table name, say) reads in both; the control is usually a plain
# message literal that CANNOT appear in `nm`. So the gate chose nm and then
# refused its own valid card for a control that was never possible there.
#
# The principle: the control's job is to prove the INSTRUMENT works on the table
# the marker was counted in, so the table must be one where a control can exist.
# Collect every discriminating table, then prefer one whose control reads on both
# images. Refusing only when NO such table exists keeps the arm as strict as it
# was without failing valid cards (PLEX, 190406a, first card to hit it).
marker_table=""
if [ "$INSTALL" -eq 1 ]; then
  # Nothing is running, so there is no staged/live differential and no control to
  # read on both images. What remains is that the marker reads in the staged
  # binary, which shows the counting instrument read this build.
  install_seen=0
  for t in strings nm; do
    ms=$(count_in "$t" "$STAGED" "$MARKER")
    say "marker $t:\"$MARKER\" staged $ms (first install: no running image to compare)"
    [ "$ms" -gt 0 ] && install_seen=1
  done
  [ "$install_seen" = "1" ] || refuse "marker reads in neither table of the staged binary, so nothing shows the instrument read this build"
elif [ "$MARKER" = "none" ]; then
  # No literal to compare. Substitute the strongest available discriminator:
  # a rebuild always moves LC_UUID, and two files sharing one are the same build.
  staged_uuid=$(dwarfdump --uuid "$STAGED" 2>/dev/null | awk '{print $2}')
  live_uuid=$(dwarfdump --uuid "$DEST" 2>/dev/null | awk '{print $2}')
  say "marker: NONE DECLARED -- this change adds no string literal"
  [ -n "$staged_uuid" ] && [ -n "$live_uuid" ] \
    || refuse "--marker none needs LC_UUID from both images and one is unreadable (not a Mach-O?); nothing has been placed"
  [ "$staged_uuid" != "$live_uuid" ] \
    || refuse "--marker none but LC_UUID is IDENTICAL ($staged_uuid): the staged artifact is the same build as the running one; nothing has been placed"
  say "identity: LC_UUID differs (staged $staged_uuid, live $live_uuid)"
  say "IDENTITY RESTS ON sidecar + placed==staged + inode; BEHAVIOUR IS UNPROVEN BY THIS GATE"
else
marker_tables=""
for t in strings nm; do
  ms=$(count_in "$t" "$STAGED" "$MARKER")
  ml=$(count_in "$t" "$DEST" "$MARKER")
  say "marker $t:\"$MARKER\" staged $ms / live $ml"
  if [ "$ms" -gt 0 ] && [ "$ml" -eq 0 ]; then marker_tables="$marker_tables $t"; fi
  # N/N in ONE table is not a refusal on its own: a JSON key that is new as a
  # string literal can share its spelling with a symbol both builds already
  # carry (PLEX, 01c23c5: `annotations` strings 1/0, nm 3/3). The literal
  # discriminates; the symbol is a different fact about a different table. What
  # keeps this honest is the control read in the SAME table the marker
  # discriminated in (below). Refusal is reserved for a marker that separates
  # the builds in NO table, which the check after this loop enforces.
  if [ "$ms" -gt 0 ] && [ "$ml" -gt 0 ]; then
    say "  note: marker also reads on both images in the $t table; that table is not evidence either way"
  fi
done
# SECOND CONTROL, PRESENT ONLY ON THE LIVE IMAGE (CEREB's design, adopted 2026-09-20
# from their cerebellum b68b31e0 card, which carried two where this gate asked for one).
#
# WHY ONE CONTROL IS NOT ENOUGH. The control above proves the counting tool can read
# both files. It does NOT prove the two reads are aimed at DIFFERENT files. Aim both
# at the staged artifact and a present-on-both control still passes -- every row then
# reads "staged N / live N" and the gate refuses the MARKER, naming the build when the
# fault is the reader. That refusal is indistinguishable from a genuinely stale stage.
#
# A needle present only on the live side cannot pass unless the live read genuinely
# reached the OLD file, so marker (staged-only) plus this (live-only) is a two-way
# proof through one instrument. The natural pick is the marker's own SUPERSEDED
# predecessor -- v10 against v12 -- which settles supersession in a single row instead
# of asserting arrival and departure separately.
if [ -n "$OLD_CONTROL" ]; then
  oc_seen=0
  for t in strings nm; do
    oc_staged=$(count_in "$t" "$STAGED" "$OLD_CONTROL")
    oc_live=$(count_in "$t" "$DEST" "$OLD_CONTROL")
    [ "$oc_live" -gt 0 ] || continue
    say "old-control $t:\"$OLD_CONTROL\" staged $oc_staged / live $oc_live"
    [ "$oc_staged" -eq 0 ] \
      || refuse "--old-control \"$OLD_CONTROL\" still reads $oc_staged in the staged artifact ($t): it is not superseded, so it cannot prove the two reads are aimed at different files"
    oc_seen=1
  done
  [ "$oc_seen" = "1" ] \
    || refuse "--old-control \"$OLD_CONTROL\" is ABSENT from the running image in both tables; 0 there means the needle is wrong or the live read is not reaching the running file, and separating those is exactly what this arm is for"
fi

[ -n "$marker_tables" ] || refuse "marker discriminates in NEITHER table: either absent from the staged artifact (dead-code-eliminated, a phrase from a comment, a string from a different binary) or present on BOTH images everywhere (a control, not a marker)"
marker_table=""
if [ -n "$CONTROL" ]; then
  for t in $marker_tables; do
    cs=$(count_in "$t" "$STAGED" "$CONTROL")
    cl=$(count_in "$t" "$DEST" "$CONTROL")
    if [ "$cs" -gt 0 ] && [ "$cl" -gt 0 ]; then marker_table="$t"; break; fi
  done
  [ -n "$marker_table" ] || refuse "the control reads on both images in NONE of the tables where the marker discriminates ($marker_tables): the marker's count is uninformative, because nothing proves the instrument can see that table at all"
else
  # --control IS REQUIRED, and the reason is the counter one layer down.
  #
  #   count_in nm  -> nm -a "$f" 2>/dev/null | grep -cF -- "$needle" || true
  #
  # That returns 0 when the needle is ABSENT and 0 when THE TOOL FAILED --
  # stderr suppressed, `|| true` swallowing the status. Measured: `nm -a` on a
  # non-Mach-O file returns 0 while `strings` on the same file returns 3.
  #
  # So a marker reading "staged 1 / live 0" is consistent with the live image
  # genuinely lacking it AND with the instrument failing on the live image. A
  # false PASS, in the direction that places a binary.
  #
  # The control closes it by construction: it must read >0 on BOTH images in the
  # marker's table, which proves the instrument can see that table on both files.
  # It was already implemented and merely OPTIONAL, so every card omitting one
  # ran with the hole open.
  #
  # This is PLEX's rule applied to my own gate (2026-09-19): name the observation
  # that CANNOT occur if the probe is working, and check for that rather than for
  # the answer. Here the impossible observation is a control reading zero on an
  # image that demonstrably contains it.
  refuse "--control is required: without a needle known to be present in BOTH images, a marker reading 0 on the live image is indistinguishable from the counting tool having failed on it (nm -a on a non-Mach-O returns 0, silently). Pass --marker none for a change that adds no literal."
fi
say "marker discriminates in the $marker_table table"
fi
# Under --marker none there is no marker table to bind the control to, so the
# control's job changes: it proves the COUNTING INSTRUMENT works on both images
# (a needle known present reading >0 in strings on each), not that a marker's
# table is readable. Without this arm `--marker none --control X` accepted X
# unread, which is a control that proves nothing (found on ENGRAM c5a4c94).
if [ -n "$CONTROL" ] && [ "$INSTALL" -eq 0 ]; then
  control_table="${marker_table:-strings}"
  c_staged=$(count_in "$control_table" "$STAGED" "$CONTROL")
  c_live=$(count_in "$control_table" "$DEST" "$CONTROL")
  say "control $control_table:\"$CONTROL\" staged $c_staged / live $c_live"
  { [ "$c_staged" -gt 0 ] && [ "$c_live" -gt 0 ]; } \
    || refuse "control must read on BOTH images in the $control_table table, else the instrument itself is unproven on one of them"
fi

# --gone asserts a REMOVAL, which is the inverse of a marker: a marker asks "did the
# new thing arrive", this asks "did the old thing leave". A bare "expect 0" is
# unfalsifiable -- it passes when the field is gone, when the reader is broken, and
# when the caller typos the string. THE DEPLOYED 1 IS WHAT MAKES THE STAGED 0 MEAN
# SOMETHING: same reader, same needle, one artifact answering each way.
if [ -n "$GONE" ]; then
  for t in strings nm; do
    gs=$(count_in "$t" "$STAGED" "$GONE")
    gl=$(count_in "$t" "$DEST" "$GONE")
    [ "$gl" -gt 0 ] || continue
    say "gone $t:\"$GONE\" staged $gs / live $gl"
    [ "$gs" -eq 0 ] || refuse "\"$GONE\" still reads $gs in the staged artifact: the removal did not land"
    gone_seen=1
  done
  [ "${gone_seen:-0}" = "1" ] \
    || refuse "\"$GONE\" is absent from the RUNNING image in both tables, so its absence from the staged one proves nothing (no positive control)"
fi

# FORMAT FLOORS: an image must read every on-disk format its module has already
# rolled forward to. The module records the floors in its data tree and each card
# declares what it reads beside it; check-format-floors.sh compares them. This is
# the arm that stops a binary-only rollback onto data only newer builds can read,
# and it runs for rollback images too, whose maps are carried below.
floor_check="$(dirname "$0")/check-format-floors.sh"
if ! "$floor_check" "$MODULE" "$STAGED"; then
  exit 2
fi

# Plan all face changes before publishing either the binary or a link. The
# dry-run path uses the same collision checks and reports the intended result.
prepare_cli_face_plan

# A GATE WITH SIDE EFFECTS MUST BE EXERCISABLE WITHOUT THEM. Every arm above is a
# read; everything below mutates. Without this split the only way to test a new arm
# is to place a binary -- which is how 0.3.92 reached production outside its quiet
# window on 2026-09-15, while the author was attending to the arm's logic and not to
# what the script does after the arms pass. Remembering that it places is exactly the
# thing that failed.
if [ "$PLACE" != "1" ]; then
  print_cli_face_plan
  say "=== arms evaluated, NOTHING placed and NOTHING restarted (pass --place to mutate)"
  exit 0
fi

if [ "$INSTALL" -eq 1 ]; then
  say "=== rollback"
  say "first install: no previous binary; rolling back means removing $DEST and the module's subc.jsonc entry"
else
say "=== rollback"
ts=$(date -u +%Y%m%dT%H%M%SZ)
# Named for the binary, like the currency pointer: two binaries of one module
# placed a second apart must not produce rollbacks only a timestamp tells apart.
rb="$STAGING/$(basename "$DEST").rollback-$ts"
mkdir -p "$STAGING"
cp "$DEST" "$rb"
# THE SNAPSHOT IS COMPARED AGAINST ITS SOURCE, not against a hash taken from
# itself. Writing the sidecar from the copy and then running `shasum -c` is
# SELF-CONFIRMING: a truncated or corrupt `cp` is hashed as truncated and
# matches, so the check reports success on exactly the snapshot that cannot be
# rolled back to. That was this arm until 2026-09-18, and the comment beside it
# said "verified" -- a self-confirming claim wearing the words of a measurement.
# (FUSI found the identical shape in their stage.sh sidecar check; the general
# test is: break each thing the guard CLAIMS to catch, one at a time.)
#
# Source-vs-snapshot equality is the whole proof. The sidecar is still written,
# because it is what a later operator uses to check the file has not rotted on
# disk since -- a different question, answered at a different time.
live_digest=$(shasum -a 256 "$DEST" | awk '{print $1}')
rb_digest=$(shasum -a 256 "$rb" | awk '{print $1}')
[ -n "$live_digest" ] && [ "$live_digest" = "$rb_digest" ] \
  || refuse "rollback snapshot does not match the live binary it was copied from (live $live_digest, snapshot $rb_digest); nothing has been placed"
(cd "$STAGING" && shasum -a 256 "$(basename "$rb")" > "$(basename "$rb").sha256")
say "rollback $(basename "$rb") matches live (${live_digest%"${live_digest#????????}"}), holds: $("$rb" --version 2>&1 | head -1)"
# The rollback image keeps the format-versions map of the build it snapshots, so
# re-placing it later is checked against the floors like any card. An image
# placed before maps existed has none, and the floor check refuses it once
# floors exist, naming how to write one by hand.
if [ -f "$DEST.format-versions.json" ]; then
  cp "$DEST.format-versions.json" "$rb.format-versions.json"
fi

if [ -n "$MIGRATES" ]; then
  [ -f "$MIGRATES" ] || refuse "--migrates named $MIGRATES, which is not a file; nothing has been placed"
  # Named for the module as well as the store file: several modules keep a
  # store.db, and snapshots sharing a name could not be pruned per module.
  store_rb="$STAGING/$MODULE.$(basename "$MIGRATES").rollback-$(date -u +%Y%m%dT%H%M%SZ)"
  # sqlite3 .backup, NOT cp: the module holds the store open with a live -wal,
  # and cp captures a torn .db beside a WAL it does not include -- a snapshot
  # that restores to a state which never existed. .backup is the online backup
  # API and is WAL-correct against a running writer.
  # `mode=ro` is right HERE because --migrates runs while the module still holds
  # the store open, so a -wal exists. It is not a universal choice: on a WAL-less
  # whole-db artifact (this snapshot itself, once written) mode=ro REFUSES with
  # error 14 on some SQLite builds and `immutable=1` is the honest reader
  # instead. Flag by artifact, not by preference (CKCRED, 2026-09-19).
  #
  # THE READER IS CHOSEN BY THE ARTIFACT, and the discriminator is whether a
  # RECOVERY SIDECAR EXISTS -- not whether the module happens to be running.
  # immutable=1 promises the file cannot change, so it skips BOTH recovery
  # mechanisms (the WAL and a rollback/hot journal) and the skipped read
  # SUCCEEDS without error.
  #
  #   sidecar present  -> mode=ro, and REFUSE on failure with NO FALLBACK.
  #                       immutable=1 would succeed and silently omit every
  #                       transaction in the -wal, which is the worst possible
  #                       outcome for a rollback artifact.
  #   NO sidecar       -> the store was closed cleanly (or is a whole-db
  #                       artifact), there is nothing to replay, and
  #                       immutable=1 is the HONEST reader. mode=ro REFUSES
  #                       here with error 14 on some SQLite builds, so keying
  #                       the choice on "did mode=ro fail" blocks a legitimate
  #                       placement on a cleanly-stopped module.
  #
  # An earlier revision keyed on the failure rather than the sidecar and would
  # have refused engram's RB-2 door: module parked, store cleanly checkpointed,
  # no -wal, mode=ro error 14. The concern the no-fallback guard was written for
  # (dropping a WAL) cannot arise when there is no WAL to drop.
  # (CKCRED + CEREB, 2026-09-19.)
  # A store snapshot holds the module's whole database (vendor payloads,
  # transcripts, audit rows), so it is created owner-only. sqlite3 creates the
  # file under the process umask, which is usually 022 and would leave a copy of
  # a 0600 store readable by every account that can enter the staging dir.
  : > "$store_rb" && chmod 600 "$store_rb" \
    || refuse "could not create $store_rb owner-only; nothing has been placed"
  if [ -f "$MIGRATES-wal" ] || [ -f "$MIGRATES-journal" ]; then
    say "store has a recovery sidecar; reading mode=ro (no immutable fallback)"
    sqlite3 "file:$MIGRATES?mode=ro" ".backup $store_rb" 2>/dev/null \
      || refuse "store snapshot failed for $MIGRATES; nothing has been placed.
        A -wal or -journal is present, so a read-only open must replay it and
        could not. Start the module and re-run, or copy the .db AND its sidecar
        to a scratch dir and snapshot the copy. Do NOT reach for immutable=1:
        it would succeed and silently omit the sidecar."
  else
    say "store has NO recovery sidecar (cleanly closed); reading immutable=1"
    sqlite3 "file:$MIGRATES?immutable=1" ".backup $store_rb" 2>/dev/null \
      || refuse "store snapshot failed for $MIGRATES; nothing has been placed.
        No -wal or -journal is present and immutable=1 still could not read it,
        so the file is damaged or is not a SQLite database."
  fi
  [ -s "$store_rb" ] || refuse "store snapshot $store_rb is empty; nothing has been placed"
  [ "$(stat -f %Lp "$store_rb" 2>/dev/null || stat -c %a "$store_rb")" = 600 ] \
    || refuse "store snapshot $store_rb is not owner-only (0600); nothing has been placed"
  (cd "$STAGING" && shasum -a 256 "$(basename "$store_rb")" > "$(basename "$store_rb").sha256")
  say "store rollback $(basename "$store_rb") ($(stat -f %z "$store_rb" 2>/dev/null || stat -c %s "$store_rb") bytes)"
  say "ROLLBACK IS BINARY + STORE: this card migrates, so restoring the binary alone would meet a newer schema and refuse"
fi
fi

say "=== place"
cp "$STAGED" "$DEST.tmp" && mv "$DEST.tmp" "$DEST"
# The live binary's map travels with it, so the next rollback snapshot can carry
# it. A map left over from the previous binary would describe the wrong build,
# so it is removed when the new card has none.
staged_map=""
for m in "$STAGED.format-versions.json" \
         "$(dirname "$STAGED")/$(basename "$STAGED" | sed 's/^SIGNED\.//').format-versions.json"; do
  if [ -f "$m" ]; then staged_map="$m"; break; fi
done
if [ -n "$staged_map" ]; then
  cp "$staged_map" "$DEST.format-versions.json.tmp" && mv "$DEST.format-versions.json.tmp" "$DEST.format-versions.json"
else
  rm -f "$DEST.format-versions.json"
fi
# THE PLACED BYTES MUST EQUAL THE STAGED BYTES, and this printed a sha without
# comparing it until 2026-09-18. Found by grepping this file for the shape of
# the rollback defect fixed forty lines up rather than by anything failing --
# the same class, three lines apart, and the audit that found it took ninety
# seconds.
#
# Without it the chain has a gap: the staged artifact is verified against its
# sidecar and the destination is proven to EXECUTE, but nothing says the thing
# executing is the thing that was verified. A short write, a full disk, or a
# racing writer lands a different binary that may still run.
placed_digest=$(shasum -a 256 "$DEST" | awk '{print $1}')
staged_digest=$(shasum -a 256 "$STAGED" | awk '{print $1}')
[ -n "$placed_digest" ] && [ "$placed_digest" = "$staged_digest" ] \
  || refuse "placed bytes differ from the staged bytes (staged $staged_digest, placed $placed_digest); the destination now holds an unverified binary"
say "placed sha ${placed_digest%"${placed_digest#????????}"} (equals staged)"
# Equal bytes mean equal signature, so the placed binary carries exactly the
# flags and entitlements checked above: this placement never re-signs. A
# placement path that runs `codesign --force` after staging would strip
# hardened runtime and its exceptions, and needs its own check of the placed file.
say "warm-exec at destination: $("$DEST" --version 2>&1 | head -1)"
reconcile_cli_faces

# Rollback retention: the newest three per binary (and per module store) are
# kept, which always includes the one taken above. Without it every placement
# left a full copy behind for good; staging had grown to 16 GB of them.
prune="$(dirname "$0")/prune-rollbacks.sh"
prune_names=("$(basename "$DEST")")
[ -n "$MIGRATES" ] && prune_names+=("$MODULE.$(basename "$MIGRATES")")
if ! CK_STAGING="$STAGING" "$prune" --keep 3 --apply "${prune_names[@]}"; then
  say "WARNING: rollback prune failed; the placement itself succeeded and older rollbacks were kept"
fi

# PATH face: the operator may invoke this by name, and that resolution is what decides
# which bytes run — not the path we just wrote.
if [ -n "$PATH_FACE" ]; then
  resolved=$(command -v "$PATH_FACE" || true)
  if [ -z "$resolved" ]; then
    say "which $PATH_FACE: not on PATH -- nothing resolves it by name"
  elif [ "$(shasum -a 256 "$resolved" | cut -d' ' -f1)" = "$(shasum -a 256 "$DEST" | cut -d' ' -f1)" ]; then
    say "which $PATH_FACE: $resolved (same bytes as the placed file)"
  else
    say "WARNING: $PATH_FACE resolves to $resolved, which is NOT the file just placed."
    say "         An operator invoking it by name runs something other than what was verified."
    say "         Place it there too, or say why the divergence is intended."
  fi
fi

if [ "$INSTALL" -eq 1 ]; then
  say "=== next: start it"
  say "  installed, not started. Add the module's entry to subc.jsonc (back the file up first),"
  say "  then 'ck module rescan --dry-run' must list '$MODULE' as added and nothing else changed,"
  say "  then 'ck module rescan' (or the next daemon cut) starts it. A helper binary the module"
  say "  runs itself as a child needs no entry."
fi

if [ "$RESTART" -eq 1 ]; then
  say "=== restart"
  # Checked, not piped away: a refused restart leaves the module on the old
  # inode with the new bytes on disk, which reads as placed.
  if ! restart_out=$(ck module restart "$MODULE" 2>&1); then
    refuse "restart of '$MODULE' failed after placement; the new bytes are on disk but the module still runs the old inode: ${restart_out}"
  fi
  printf '%s\n' "$restart_out" | tail -1
    say "$(date -u +%FT%TZ) restart initiated -- verify on a lane opened AFTER this point:"
    # THE PID COMES FROM PROVENANCE, NOT FROM `pgrep` AND NOT FROM `module status`.
    #
    #   ck module status -> SupervisorEntry, 17 fields, NO pid. It answers
    #       supervision state (enabled, live, restart budget, drain policy).
    #       A pid is an OBSERVED PROCESS FACT, which is what the provenance
    #       surface holds, beside spawned_from and running_image. Reading
    #       `.module.pid` there returns null -- and a nonexistent key and a
    #       null value are the same bytes, so it reads as "no process".
    #
    #   pgrep -> on macOS EXCLUDES THE CALLER'S ANCESTORS by default, so from
    #       a shell this daemon supervises it structurally cannot return the
    #       daemon. That produced a false inode MISMATCH on the 0.18.14 cut.
    #
    # Printed here rather than remembered, because both wrong answers look
    # like findings about the module.
    say "  ck --json provenance $MODULE | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"modules\"][0][\"daemon_observed\"][\"pid\"])'"
    say "  then inode proc-vs-disk on that pid, which is the only proof it runs these bytes"
    # LOGS: TWO FILES, TWO QUESTIONS. The module's own r2 sink is where a
    # current line lands; the daemon's stderr capture is a frozen archive
    # once a module adopts fleet logging, and it can only hold or grow --
    # nothing removes lines from it. A grep there returns a true count of
    # HISTORICAL lines that reads exactly like a fresh one (broca's five
    # un-timestamped seal lines, all pre-adoption, read as "newest" on the
    # 0.3.106 card). So: read the event from the r2 sink, and read only the
    # SIZE of the archive, which must not move.
    say "  logs: event in the module's r2 sink ~/.local/share/cortexkit/$MODULE/logs/$MODULE.<YYYY-MM-DD>.log (timestamped)"
    say "        archive ~/.local/share/cortexkit/run/logs/$MODULE.stderr.log must not GROW ($(wc -c < ~/.local/share/cortexkit/run/logs/$MODULE.stderr.log 2>/dev/null || echo 0) bytes now); a line found there is historical"
fi
