# app-state-review/v1

Review artifact for cerebellum's application-state read (`computer.read_state`, key
`CEREBELLUM_APP_STATE_REVIEW_REVISION`): an application's windows, sheets and menu bar as a text
tree with element ids that persist across reads, the diff between reads, and the optional window
image taken through capture custody.

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

## What the review read (live arms, reply and readback checked)

- `read_ids`: element ids persist across reads and name live elements.
- `discord_background_read`: a covered Chromium-based window is read without bringing it forward.
- `ungranted_app`: an application with no grant is refused before any read.
- `identity_recheck`: the application's identity and binary digest are re-checked before the
  read, and a mismatch refuses.
- `takeover_other_conversation`: a grant held by one conversation serves no other.
- `always_grant_signers`: Apple-signed, Team ID and ad hoc re-signed applications each get a
  conversation-only grant.
- `consent_unanswered_never_acts`: an unanswered consent question parks for 120 s, ends refused,
  and a late answer grants for later calls without running the original one.

Not measurable live, covered by unit fences: `consent_expiry` (no question-deadline seam).
