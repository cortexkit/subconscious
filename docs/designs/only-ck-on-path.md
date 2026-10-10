# Only `ck` on PATH

Fresh CortexKit installs put only `ck` (`ck.exe` on Windows) on PATH, from the
`<data home>/cortexkit/cmd/` directory. Managed modules and domain binaries
remain in `<data home>/cortexkit/bin/` and are reached through `ck <name>`.

Existing installs are not migrated automatically. Owners of those installs
move their existing PATH entries by hand if they want only `ck` on PATH.
