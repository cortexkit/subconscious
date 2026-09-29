# private-input-route-review/v1

Review artifact for background keystrokes through macOS's private SkyLight posting function (key
`CEREBELLUM_PRIVATE_INPUT_ROUTE_REVIEW_REVISION`, behind the `private_input_route` switch).

Minted by subc after an evidence review of the v13 desk run: `ck-cerebellum` at commit
`ca6b54d22bddd8c1acc57096d8b8c3097208c1eb` (cerebellum origin/main, CI green), built as a
hardened-runtime copy (`flags=0x10002(adhoc,runtime)`, no entitlements) and run against an isolated
scratch module and journal, 224 s, 2026-09-29. Run log: cerebellum
`.cortexkit/alfonso/desk/v13-desk-run-2026-09-29-hardened.log`, sha256
`453af4a9a509af91c3342e6901b412780c8af81fec8e5032b3f2a70aaf9e45b8`. It is kept outside this public
repository because its state reads contain real application text. Cerebellum binds its
release-build review revision to this revision by name.

The review covers the code at that commit. A build from a later commit that changes this key's
code paths needs a new revision; the placement card names the diff from `ca6b54d` for these paths.

## What the review read

- `route_recorded` (live): text typed into a background Discord through the private route,
  recorded per event in the journal and confirmed by readback 3.6 s after the call.
- `cancel_mid_type` (live): a cancel mid-sequence stops at the next post and leaves no key held.
- Route selection: the public route is used only when the private function is absent; a runtime
  attach or lookup failure refuses the keystroke, never falls back.
  `symbol_missing` and `auth_attach_failure` are not forceable in a release build and are covered
  by unit fences.

Not covered here: the fronting bracket for keys and type into a background application, which is
the takeover-dispatch key and is not reviewed by this revision.
