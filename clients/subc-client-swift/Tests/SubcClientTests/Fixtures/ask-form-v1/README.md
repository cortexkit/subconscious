# Ask form v1

Tag: `ask-form-v1`. This directory is the client contract for form asks: one
ask carrying up to 6 typed questions, answered on one phone page. It also holds
the one-question baseline that forms must not change.

## Legacy baseline (read-only)

These legacy fixtures were captured before form implementation from
`9c1e7337154b5932621852d443392baa3a0cbbb1`. The capture added only test code;
the store, dispatch, projection and agent notification code were unchanged.

All three files describe the same single-with-options user ask:

- `legacy-records.json`: the `ask.get` projection before and after answering,
  plus the open `ask.list_pending_for_user` reply.
- `legacy-replies.json`: the complete `ask.persist_answer` success reply.
- `legacy-answer-text.json`: the actual agent notification queued by the
  runtime's answered-user-ask delivery pass, including its system-reminder
  envelope. This is not a test-side reconstruction of the renderer.

The test seeds a real migrated store with fixed identifiers, options, a
recommended choice, a default decision and timestamps. It supplies a block
silence policy directly through the store so local autonomy configuration
cannot affect the bytes. It then uses the real dispatch operations and a
registered Pi plugin session to capture the queued notification text. Only
the text is compared; registration timestamps and scratch paths are not part
of the contract.

The read-only regression test is
`ask_form_legacy_vector_tests::ask_form_legacy_single_vectors_are_read_only`
in `crates/prefrontal-core-module/src/main.rs`.

Nothing writes any `legacy-*.json` file, including form regeneration mode. Do
not regenerate or edit these files: a changed legacy result is a compatibility
regression, not a reason to update the baseline.

## Form files (generated)

- `records.json`: `ask.get` replies for an open form using every kind (with and
  without `subtitle`, options with and without `detail`), the open
  `ask.list_pending_for_user` reply, a form answered with mixed `person` and
  `recommended` sources, a form resolved on silence, a two-round form after
  its follow-up and after its round-2 answer, and a form holding a question of
  an unknown kind before and after answering.
- `replies.json`: the `ask.record` `invalid_form` refusal, and the
  `ask.persist_answer` success, `conflict` and `invalid_answers` replies. It
  also holds the `wrong_type` refusal for a value sent to the unknown-kind
  question.
- `answer-text.json`: the answer block stored in `answer` for the answered,
  silence-resolved, two-round and unknown-kind forms. The answered block
  includes a multiline option label and a multiline `text` value.
- `push.json`: the form delivery push plaintext, decoded and as the exact
  sealed string. One push carries a veto-window `deadlineAtMs`. The other has
  a 172-scalar title, which the push cuts to 149 scalars plus `…`.

The producer test is
`ask_form_legacy_vector_tests::ask_form_v1_form_vectors_are_produced_from_real_code`
in `crates/prefrontal-core-module/src/main.rs`. It records forms through the
store with fixed silence policies, ids and timestamps, so local autonomy
configuration cannot affect the bytes. It answers, resolves and refuses
through the real dispatch operations, and opens the push sealed by the
manager's real delivery path. Only the unknown-kind definition is written
through the store's test helper, because today's validator refuses that kind.

The producer first checks the legacy bytes. If they changed, it fails before
writing anything, even with `UPDATE_ASK_FORM_VECTOR=1`.

```sh
cargo nextest run -p prefrontal-core-module --lib --bins -E 'test(/ask_form_legacy_single_vectors_are_read_only|ask_form_v1_form_vectors_are_produced_from_real_code/)'
UPDATE_ASK_FORM_VECTOR=1 cargo nextest run -p prefrontal-core-module --lib --bins -E 'test(ask_form_v1_form_vectors_are_produced_from_real_code)'
```

Regeneration rewrites the four form files and `SHA256SUMS`. Both tests
compare exact pretty-printed JSON bytes, including the final newline.

## Checksums

`SHA256SUMS` lists every legacy and form JSON file; this README is not listed.
The producer test fails if the manifest is missing a file or a digest is
stale. Verify it with `shasum -a 256 -c SHA256SUMS` from this directory.
