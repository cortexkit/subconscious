#!/usr/bin/env bash
# Diff the DEPLOYED subc.jsonc module roster against docs/fleet-manifest.json,
# both directions. The manifest is the fleet's committed denominator (E2E's
# gate and other CI consumers derive from it, since deployed config does not
# exist on runners); this check runs where the deployed state lives, so drift
# alarms at the seat that can fix it. Either direction is a finding: deployed-
# not-in-manifest means CI consumers cannot see a live module; manifest-not-
# deployed means the manifest describes a fleet that no longer runs.
set -uo pipefail
CFG="${SUBC_CONFIG:-$HOME/.config/cortexkit/subc.jsonc}"
MANIFEST="$(dirname "$0")/../../docs/fleet-manifest.json"
[ -f "$CFG" ] || { echo "SKIP: no deployed config at $CFG (CI runner?)"; exit 0; }
deployed=$(python3 -c "
import json,re,sys
raw=open('$CFG').read()
raw=re.sub(r'//[^\n]*','',raw); raw=re.sub(r',\s*([}\]])',r'\1',raw)
print('\n'.join(sorted(json.loads(raw).get('modules',{}).keys())))")
manifest=$(python3 -c "
import json
print('\n'.join(sorted(json.load(open('$MANIFEST'))['modules'].keys())))")
only_deployed=$(comm -23 <(echo "$deployed") <(echo "$manifest"))
only_manifest=$(comm -13 <(echo "$deployed") <(echo "$manifest"))
n_dep=$(echo "$deployed" | grep -c .)
[ "$n_dep" -lt 1 ] && { echo "VACUOUS: zero deployed modules parsed" >&2; exit 2; }
ok=0
[ -n "$only_deployed" ] && { echo "DEPLOYED-NOT-IN-MANIFEST (CI-blind modules): $only_deployed"; ok=1; }
[ -n "$only_manifest" ] && { echo "MANIFEST-NOT-DEPLOYED (stale manifest rows): $only_manifest"; ok=1; }
echo "deployed=$n_dep manifest=$(echo "$manifest" | grep -c .) drift=$([ $ok -eq 0 ] && echo none || echo YES)"
exit $ok
