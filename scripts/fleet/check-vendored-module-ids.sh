#!/usr/bin/env bash
# Fail when a VENDORED SIGNED CORPUS names a module that does not exist.
#
# WHY A PLAINTEXT SCAN CANNOT DO THIS. The module ids live in the
# `federation_exposure` claim INSIDE JWS payloads, so they are base64url-encoded
# on disk. `grep cortexkit-credentials docs/` returns zero on a file carrying
# eight of them. That is exactly how the module renames missed these corpora:
# everything greppable was updated, and the signed claims were structurally
# invisible to the sweep. This decodes the payloads instead.
#
# WHY THIS LIVES ON THE CONSUMER SIDE. A producer cannot know who vendored it --
# it can only notify whoever it happens to think of, which is a courtesy rather
# than a mechanism. A consumer CAN enumerate its own repository exhaustively.
# So the durable check is here, and it needs no notice from anyone.
#
# WHY AN ALLOWLIST RATHER THAN A DENYLIST OF RETIRED NAMES. A denylist passes
# for any id nobody thought to add, so it fails OPEN on precisely the next
# rename -- the failure that produced this check.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1

# The allowlist is derived from the running daemon, which makes this check a
# function of THE MACHINE IT RUNS ON. A module that is not yet deployed, or is
# deliberately disabled in this daemon's config, reads as unknown and fails a
# corpus that is actually correct. That is the right failure direction -- fail
# closed on an id we cannot confirm -- but it answers "known to THIS daemon"
# rather than "exists in the fleet". If this fires on an id you believe is real,
# check the daemon's config before editing the corpus.
LIVE=$(ck module list 2>/dev/null | awk 'NR>1 {print $1}' | tr '\n' ' ')
if [ -z "${LIVE// }" ]; then
  echo "REFUSING: could not read the live module set from the daemon."
  echo "  An empty allowlist would fail every id, which is noise rather than a finding."
  exit 2
fi

python3 - "$LIVE" <<'PY'
import base64, glob, json, re, sys

live = set(sys.argv[1].split())
# A deliberate synthetic placeholder in the bounds/malformed vectors. It must
# never track a real module, so it is excluded from both counts rather than
# being allowlisted.
SYNTHETIC = {"fixture-module"}

matched = 0
decoded = 0
skipped: list[tuple[str, str]] = []
found: dict[str, int] = {}
offenders: list[tuple[str, str]] = []

def payload(tok: str):
    body = tok.split(".")[1]
    body += "=" * (-len(body) % 4)
    return json.loads(base64.urlsafe_b64decode(body))

# The third segment has NO MINIMUM LENGTH, deliberately.
#
# It used to require 10+ characters, which silently assumed every token is
# SIGNED. An `alg:none` token has an EMPTY third segment, so the narrow form
# missed it entirely -- and a corpus carrying one would pass this check while
# containing a dead module id, which is precisely the failure the check exists
# to catch.
#
# I had recorded that as a known limit on a reachability argument: nothing in
# our corpora emits unsigned tokens, so the case was unreachable. That was the
# wrong frame. I priced it as ADDING A SPECIAL CASE for a shape nobody emits,
# when it is REMOVING AN ASSUMPTION I had no reason to make. Measured on the
# clean corpus both forms find exactly 92 tokens, so the widening costs nothing
# and closes the hole. CKCRED and CALLO reached the same correction on their
# scanners independently.
TOKEN_RE = r"[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]*"

def walk_strings(node):
    """Yield every string value in a parsed JSON document.

    A SECOND DISCOVERY MECHANISM WITH A DIFFERENT BLIND SPOT FROM THE REGEX
    SWEEP, which is the only thing that makes a second count worth having.
    Two counts derived from one mechanism move together and agree while both
    are wrong -- they duplicate rather than cross-check.

    The two differ concretely: this walk sees a token that is a string value in
    its own right and MISSES one embedded inside a longer string; the raw-byte
    sweep below sees both. So a token the walk skips still lands in the sweep,
    the counts disagree, and the mismatch fires. Neither mechanism checks the
    OTHER's shape assumption -- a token in a form matching neither (CKCRED and
    CALLO constructed an `alg:none` with an empty third segment) is invisible to
    both, and that limit is known rather than closed.
    """
    if isinstance(node, str):
        yield node
    elif isinstance(node, dict):
        for v in node.values():
            yield from walk_strings(v)
    elif isinstance(node, list):
        for v in node:
            yield from walk_strings(v)

walk_seen = 0
sweep_seen = 0

for path in sorted(glob.glob("docs/**/*.json", recursive=True)):
    text = open(path).read()
    sweep_seen += len(re.findall(TOKEN_RE, text))
    try:
        walk_seen += sum(1 for s in walk_strings(json.loads(text)) if re.fullmatch(TOKEN_RE, s))
    except Exception:
        # An unparseable document is not a document without tokens. Count it as
        # a walk failure rather than as zero, so the mismatch below fires.
        pass
    for tok in re.findall(TOKEN_RE, text):
        matched += 1
        try:
            claims = payload(tok)
        except Exception as exc:
            # A TOKEN THIS SCAN CANNOT READ IS NOT A TOKEN THAT IS CLEAN.
            # This arm used to `continue` in silence, which meant a fixture
            # minted in any shape the decoder does not handle would be walked
            # past while the rest of the corpus still produced a pass. That is
            # the found-SOMETHING vs found-EVERYTHING gap: recognising ids in
            # the tokens it can read says nothing about the ones it skipped,
            # and a corpus that passes because the offending token was never
            # visited is indistinguishable from a clean one.
            skipped.append((path, str(exc)[:60]))
            continue
        decoded += 1
        for mod in re.findall(r'"module"\s*:\s*"([^"]+)"', json.dumps(claims)):
            if mod in SYNTHETIC:
                continue
            found[mod] = found.get(mod, 0) + 1
            if mod not in live:
                offenders.append((path, mod))

recognised = sum(c for m, c in found.items() if m in live)

# Report matched and decoded SEPARATELY. A single "tokens" number cannot show
# the gap between what the scan found and what it could actually read.
print(f"  tokens matched: {matched}  (sweep {sweep_seen} / walk {walk_seen})")
print(f"  tokens decoded: {decoded}")
print(f"  recognised ids: {recognised}")
print(f"  unknown ids:    {len(offenders)}")

if sweep_seen != walk_seen:
    print(f"FAIL: the two discovery mechanisms disagree ({sweep_seen} vs {walk_seen}).")
    print("  One of them is missing tokens the corpus contains. This is a true")
    print("  positive about the SCAN, not about the corpus -- fix the scan first.")
    sys.exit(1)

if skipped:
    print(f"FAIL: {len(skipped)} token(s) matched but could not be decoded.")
    print("  The scan cannot vouch for a token it never read.")
    for path, why in skipped[:5]:
        print(f"  {path}: {why}")
    sys.exit(1)

# THE POSITIVE CONTROL, AND IT IS THE REASON THIS SCRIPT IS TRUSTWORTHY.
# "unknown = 0" is produced BOTH by a clean corpus and by a decoder that silently
# matches nothing -- and the second is what happened when this check was first
# written by hand: it reported 16 dead and 0 live, having never once demonstrated
# it could see a CORRECT id. A corpus with no recognised ids is therefore treated
# as a broken instrument, not as a pass.
if decoded and recognised == 0:
    print("FAIL: decoded tokens but recognised zero known module ids.")
    print("  Treat this as a BROKEN DECODER rather than a clean corpus:")
    print("  a scan that cannot see a correct id cannot report a wrong one.")
    sys.exit(1)

if offenders:
    print("FAIL: vendored corpora name modules the daemon does not know:")
    for path, mod in offenders:
        print(f"  {path}: {mod}")
    sys.exit(1)

print("OK: every signed module id is a live module.")
PY

rc=$?
[ $rc -ne 0 ] && exit $rc

# --- AUTHENTICITY, WHICH IS A DIFFERENT QUESTION FROM EVERYTHING ABOVE -------
#
# Everything up to here asserts properties of the bytes IN HAND: the ids are
# live, the tokens decode, the two discovery mechanisms agree. None of it can
# tell whether these bytes are the ones the producer published -- a hand-edit
# that keeps every id live passes all of it.
#
# docs/team-mode/VENDORED.md records per-file digests, but comparing a file to a
# digest I wrote down myself proves INTERNAL CONSISTENCY, not authenticity: it
# confirms the bytes match my own record of them. PLEX hit exactly this shape in
# their acceptance script -- an embedded lock digest that matched the lock at the
# commit the binary CLAIMED, which reads like an answer to "is this the build we
# mean" and is not one.
#
# The stronger check is available whenever the producer repo is on this machine:
# compare against the artifact at the named commit. When it is not available this
# SAYS SO rather than passing quietly, because a check that silently degrades to
# a weaker one reports the same success either way.
PRODUCER=~/Work/Projects/CortexKit/cortexkit-account
COMMIT=$(grep -oE '`[0-9a-f]{40}`' docs/team-mode/VENDORED.md | head -1 | tr -d '`')

if [ -z "$COMMIT" ]; then
  echo "AUTHENTICITY: NOT CHECKED -- no source commit recorded in VENDORED.md"
  exit 0
fi

if [ ! -d "$PRODUCER/.git" ]; then
  echo "AUTHENTICITY: NOT CHECKED -- producer repo absent at $PRODUCER"
  echo "  The checks above verify the ids, not that these bytes are the published ones."
  exit 0
fi

# THE PAIR LIST IS TRANSCRIBED, SO IT CAN GO STALE SILENTLY.
#
# A fifth vendored corpus added under docs/team-mode is checked for LIVE IDS by
# the scan above -- it discovers files itself -- but is invisible to the
# authenticity loop below, which only knows what is written here. The success
# line would still say "all 4", which is a TRANSCRIBED COUNT rather than a
# measured one: it reports coverage it never established.
#
# So the list is cross-checked against what the scan actually found. Any file
# carrying tokens that is absent from the list fails, rather than being silently
# skipped while the summary implies it was covered.
CORPORA=$(python3 - <<'PY'
import glob, re
TOK = r"[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]*"
for p in sorted(glob.glob("docs/**/*.json", recursive=True)):
    if re.search(TOK, open(p).read()):
        print(p)
PY
)

LIST="room1-contract-samples.json:docs/team-mode/fixtures/room1-contract-samples.mirror.json
room1-contract-samples.json:docs/team-mode/conformance/vectors/ckcred/room1-contract-samples.json
room2-contract-samples.json:docs/team-mode/fixtures/room2-ckcred-fixtures.json
room2-contract-samples.json:docs/team-mode/appendices/room2-ckcred-fixtures.json"

fail=0
for pair in $LIST; do
  name=${pair%%:*}; mine=${pair#*:}
  src=$(cd "$PRODUCER" && git show "$COMMIT" --stat --name-only 2>/dev/null | grep "/$name\$" | head -1)
  if [ -z "$src" ]; then
    echo "AUTHENTICITY: $name not found at $COMMIT in the producer repo"
    fail=1; continue
  fi
  want=$(cd "$PRODUCER" && git show "$COMMIT:$src" | shasum -a 256 | cut -d' ' -f1)
  got=$(shasum -a 256 "$mine" | cut -d' ' -f1)
  if [ "$want" != "$got" ]; then
    echo "AUTHENTICITY FAIL: $mine does not match the producer at $COMMIT"
    fail=1
  fi
done

# Every corpus the scan found must appear in the list above. Without this, the
# only signal that a new mirror went unchecked is that nobody remembered to add
# it -- which is not a signal.
checked=0
for corpus in $CORPORA; do
  checked=$((checked + 1))
  case "$LIST" in
    *"$corpus"*) ;;
    *)
      echo "AUTHENTICITY FAIL: $corpus carries tokens but is not in the pair list."
      echo "  It was checked for live ids and NOT for authenticity."
      fail=1
      ;;
  esac
done

if [ $fail -ne 0 ]; then
  exit 1
fi
echo "OK: $checked mirrors byte-identical to the producer at ${COMMIT:0:8}."
