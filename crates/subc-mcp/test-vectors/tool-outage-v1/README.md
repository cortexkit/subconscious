# Tool-outage samples

`valid-error-outputs.json` is copied byte for byte from the
`cortexkit-role-llm-runner` 0.7.9 crate's published tool-outage test vectors.
Each sample is an outage error with every member valid, and `rendered` is the
crate's own rendering of it. The gateway's copy of that rendering
(`src/outage_output.rs`) must reproduce every `rendered` exactly.

`SHA256SUMS` matches the crate's published checksum for the file. Replace the
file only with a newer published copy, and update the checksum with it.
