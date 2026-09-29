# takeover-dispatch-review/v1

Review artifact for cerebellum's takeover dispatch (key `CEREBELLUM_TAKEOVER_DISPATCH_REVIEW_REVISION`):
anything that can take the person's input focus. That is the takeover pointer rows
(`computer.move`, `computer.drag`, `computer.scroll`) and the fronting bracket `computer.type` and
`computer.keys` use to reach an application that is not frontmost: bring it forward for at most
100 ms at a time, post, put the previous application back, under the idle gate and the seize
watcher.

Minted by subc against `ck-cerebellum` at commit `abe8e3aa16d6e0e7c72af00cad066be3e9614c4a`
(cerebellum origin/main, gate and CI green). The live evidence comes from the v13 desk run at
`ca6b54d` (see `app-state-review-v1.md`: hardened copy, scratch module and journal, log sha256
`453af4a9a509af91c3342e6901b412780c8af81fec8e5032b3f2a70aaf9e45b8`). The only change to these
paths since then is `abe8e3a`'s effect-status rule, reviewed from its diff below. Cerebellum binds
its release-build review revision to this revision by name.

## What the review read

Live, on the v13 desk run:
- `drag_scroll`: pointer rows through the takeover path, reply and readback matched.
- `cancel_mid_type`: a cancel mid-sequence stops at the next post and leaves no key held.
- `chord_background`: `command+a` into a background TextEdit. When TextEdit took longer than the
  100 ms bracket to come forward, the reply was `fronting_bound_exceeded` with `posts_returned: 0`
  and Finder restored: nothing was posted and the foreground was put back. The same arm passed in
  3 of the 4 runs that reached it on 2026-09-29. The 100 ms bound is kept: it is what protects the
  person's foreground, and a refusal that posted nothing is safe to retry once the status below
  says so.
- `takeover_other_conversation`: a takeover grant held by one conversation serves no other.

From the diff `8e661074..abe8e3a` (`computer_capability/keystrokes.rs` and `mod.rs`):
- **The status is decided from whether a post call was made, not from a zero count.**
  `post_call_made` answers `Some(false)` only when no post call was attempted: a bracket or
  sequence report with zero returned posts, no failed post call, and no posted release of held
  keys. A first post call that failed stays `effect_indeterminate` at zero, because the event may
  have gone out. Any refusal whose report carries no count answers `None` and stays indeterminate.
- **Every refusal returned before dispatch is marked `not_dispatched`:** parse errors, blocked
  chords, consent refusals and cancels, focus and secure-field refusals, the modifier-chord and
  needs-foreground refusals, the app restarting while the person was asked, a missing dispatch
  row, attendance unavailable, and the screen locked. An unmarked path keeps
  `effect_indeterminate`, which is the safe default for code added later.
- **The mark is applied after the restore disclosure,** so a bracket that failed to put the
  previous application back without posting still reports both facts.
- **Reader rule:** a value of `effect_status` a reader does not know is read as
  `effect_indeterminate`, never as not-run. It is stated in the v14 rows and pinned by
  `effect_status_unknown_value.json` and `an_unknown_effect_status_is_read_as_indeterminate_by_rule`.
- **Mutations, each failing a named test:** always indeterminate; `not_dispatched` regardless of
  posts; the literal zero-count rule (which would misreport a failed first call); every refusal
  marked `not_dispatched`.

Prefrontal's relay passes the reply through unchanged and does not branch on `effect_status`, so
no reader has to learn the new value before it ships.
