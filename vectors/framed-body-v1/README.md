# Framed body v1

The body is `u32 big-endian H | H bytes JSON object | opaque payload`.
The head is nonempty and at most 65,536 bytes. Payloads may be empty and are
never interpreted by the codec. The declared operation limit is 1 through
8,388,608 bytes, independently of the peer's whole-frame inbound limit.

`fed-call.bin` wraps `small.bin` in a fed-wire call with `framed: true`;
`fed-call-header.json` is its exact JSON header (without the final newline).
The outer fed-wire lengths are little-endian, unlike the inner head length.
This call requires the negotiated `mgmt-v1` and `framed-calls-v1` features.

`cases.json` indexes valid bodies (expected parsed head and exact payload hex),
invalid bodies (expected `FramedBodyError` variant name), and manifest cases
(expected `OperationPayloadError` variant name or null for acceptance).
`max-head.bin` has H exactly 65,536. `opaque-payload.bin` contains NUL and invalid
UTF-8. `truncated-head.bin` contains a valid object but declares one extra byte,
so clamping the length must not accidentally accept it.

Payload declarations live in the manifest's `operation_payload` map keyed by
management operation name, not in the exhaustive operation descriptor. A key
must name a declared management operation and cannot name a watch-delivery
operation. Old manifests omit the map and preserve their bytes.

Regenerate with `python3 vectors/framed-body-v1/generate.py`; verify without
writing with `python3 vectors/framed-body-v1/generate.py --check`.
`SHA256SUMS` covers all sources and generated files except itself. The generator
uses only Python's standard library and no production codec. Rust and Swift
consume these same binary and manifest files.
`requirements.txt` intentionally has no dependencies; it satisfies the shared
CI job's uniform install step.
