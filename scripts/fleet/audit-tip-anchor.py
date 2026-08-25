#!/usr/bin/env python3
"""External truncation witness for the claustrum audit chain.

Reads a `ck health claustrum --json` payload on stdin, maintains an
append-only JSONL anchor file, and alarms on the two signatures a
hash-chained audit log cannot see about itself (verify-audit proves
prefix validity; a deleted tail is a shorter VALID chain):

  TRUNCATION  -- observed auditSeq below the highest ever recorded.
  REWRITE     -- observed auditSeq matches a recorded seq with a
                 DIFFERENT mac. This is the sharper check: truncation
                 followed by fresh appends RETURNS seq to old values,
                 so monotonicity alone passes it; mac-at-seq stability
                 is what binds bytes rather than counts.

HONEST BOUND (CKCRED's, kept verbatim): at pulse granularity the anchor
catches truncation that PERSISTS ACROSS a pulse. A truncation fully
repaired between two samples -- same rows reappended to the same seq --
leaves no observable difference at either sample. The anchor bounds the
window; it does not close it. Backups (engram generations) are the
slow-window complement.

Exit codes: 0 recorded/no-change, 2 field absent (named), 3 ALARM.
"""

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

ANCHOR_DEFAULT = Path.home() / ".local/share/cortexkit/fleet-pulse/claustrum-audit-tip.jsonl"


def main() -> int:
    anchor_path = Path(sys.argv[1]) if len(sys.argv) > 1 else ANCHOR_DEFAULT
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError as err:
        print(f"ANCHOR SKIP: health payload unparsable ({err}) -- instrument, not chain state")
        return 2
    metrics = payload.get("metrics", {})
    seq = metrics.get("auditSeq")
    # auditTipMac is canonical (CKCRED ruling, live at 140485a; the shipped
    # d0e8709 wire briefly served entryMac and that alias is deliberately NOT
    # decoded here -- a re-divergence must trip the absent arm rather than be
    # absorbed by a leftover fallback).
    mac = metrics.get("auditTipMac")
    if seq is None or mac is None:
        # Absent is a defined state, not an error: empty chain, unreadable
        # store, or a claustrum binary predating the field. Name which.
        if metrics.get("storeReadable") is False:
            print("ANCHOR SKIP: store unreadable -- tip omitted by contract")
        elif metrics:
            print("ANCHOR SKIP: auditSeq/auditTipMac absent (empty chain or binary predates d0e8709)")
        else:
            print("ANCHOR SKIP: no metrics in payload")
        return 2

    anchor_path.parent.mkdir(parents=True, exist_ok=True)
    history = []
    if anchor_path.exists():
        with anchor_path.open() as fh:
            for line in fh:
                line = line.strip()
                if line:
                    history.append(json.loads(line))

    alarms = []
    max_seen = max((h["seq"] for h in history), default=None)
    if max_seen is not None and seq < max_seen:
        alarms.append(
            f"AUDIT-CHAIN ALARM (truncation): observed seq {seq} below recorded max {max_seen}"
        )
    for h in history:
        if h["seq"] == seq and h["mac"] != mac:
            alarms.append(
                f"AUDIT-CHAIN ALARM (rewrite): seq {seq} previously recorded with mac {h['mac'][:16]}.., now {str(mac)[:16]}.. -- truncate-and-reappend signature"
            )
            break

    # Alarmed observations are NOT appended: the anchor file is the trusted
    # baseline, and recording adversarial bytes into it would make the next
    # LEGITIMATE tip alarm against the attacker's mac (observed in the drive
    # that built this script). A persisting bad state re-alarms every pulse
    # against untainted history instead; the pulse transcript witnesses the
    # alarm itself.
    if alarms:
        for alarm in alarms:
            print(alarm)
        return 3

    last = history[-1] if history else None
    if last is None or last["seq"] != seq or last["mac"] != mac:
        entry = {
            "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "seq": seq,
            "mac": mac,
        }
        with anchor_path.open("a") as fh:
            fh.write(json.dumps(entry, separators=(",", ":")) + "\n")
        recorded = "recorded"
    else:
        recorded = "unchanged"

    print(f"anchor {recorded}: seq {seq} mac {str(mac)[:16]}.. ({len(history)} prior observations)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
