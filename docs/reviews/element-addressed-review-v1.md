# element-addressed-review/v1

Review artifact for acting on an element through accessibility (key
`CEREBELLUM_ELEMENT_ADDRESSED_REVIEW_REVISION`): `computer.press_element`,
`computer.press_point` (hit-tested within the returned image's own application; the pointer never
moves) and `computer.write_text` (written, then read back).

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

- `read_ids`: a stale element id refuses `stale_element` with no actuation row and the target
  unchanged.
- `coordinate_covered_acts_on_target_only`: a point press on a covered window reaches the element
  of the approved application the agent was shown, through accessibility, never a real click.
- `write_text_web`: typing into web content is refused.
- `identity_recheck`, `ungranted_app`, `takeover_other_conversation`: as in app-state-review/v1.

Not measurable live, covered by unit fences: `coordinate_no_target_element` (TextEdit reports an
element outside its window, so there is no clean miss point).
