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

for path in sorted(glob.glob("docs/**/*.json", recursive=True)):
    text = open(path).read()
    for tok in re.findall(r"[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{10,}", text):
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
print(f"  tokens matched: {matched}")
print(f"  tokens decoded: {decoded}")
print(f"  recognised ids: {recognised}")
print(f"  unknown ids:    {len(offenders)}")

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
