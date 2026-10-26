# Rendezvous device-approval notices v1.0

`rdv-notices-v1.0.jsonl` is the rendezvous server's published set, copied
byte for byte. Its ten signed notices cover approval, removal and merge pending
and resolved payloads, both device rosters, and enrollment predecessors. Each
line includes the account verification key, signed frame and receipt-only ack.
There is no local generator.

Verify the pinned bytes from this directory with `shasum -a 256 -c SHA256SUMS`.
`DeviceApprovalNoticeTests` checks the checksum, every signature, acknowledgement,
and locally derived comparison fingerprint/code. Test keys are not production keys.
