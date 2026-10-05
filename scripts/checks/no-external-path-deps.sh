#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/../.." && pwd -P)
manifest_path="$repo_root/Cargo.toml"

if (($# > 1)); then
  printf 'usage: %s [manifest-path]\n' "${BASH_SOURCE[0]}" >&2
  exit 2
fi
if (($# == 1)); then
  manifest_path="$1"
fi

metadata_status=0
metadata=$(cargo metadata --format-version 1 --locked --manifest-path "$manifest_path") || metadata_status=$?
if ((metadata_status != 0)); then
  printf 'check itself failed: cargo metadata exited with status %s\n' "$metadata_status" >&2
  exit 2
fi

if printf '%s\n' "$metadata" | python3 -c '
import json
import os
import sys

metadata = json.load(sys.stdin)
workspace_root = os.path.realpath(metadata["workspace_root"])
packages = metadata.get("packages", [])
path_packages = [package for package in packages if package.get("source") is None]
offenders = []

print(
    f"cargo metadata examined {len(packages)} packages "
    f"({len(path_packages)} path packages)"
)
if not packages:
    print("error: cargo metadata returned zero packages", file=sys.stderr)
    sys.exit(2)

for package in path_packages:
    manifest_path = os.path.realpath(package["manifest_path"])
    try:
        if os.path.commonpath((workspace_root, manifest_path)) == workspace_root:
            continue
    except ValueError:
        # Different drives cannot be contained by the workspace root.
        pass
    offenders.append((package["name"], package["version"], manifest_path))

if offenders:
    for name, version, manifest_path in offenders:
        print(
            f"external path dependency: {name} {version} ({manifest_path})",
            file=sys.stderr,
        )
    sys.exit(3)

print(f"no external path dependencies found in {workspace_root}")
'
then
  exit 0
else
  parser_status=$?
  if ((parser_status == 3)); then
    exit 3
  fi
  printf 'check itself failed: metadata parser exited with status %s\n' "$parser_status" >&2
  exit 2
fi
