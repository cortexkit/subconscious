# Changelog

## 0.1.1

- A profile no longer reports `truncated: true` because an idle thread's deep wait stack was cut. Idle threads are dropped from the reply whole, so cutting their stacks removes nothing the reply would have shown. `truncated` now means a reported thread, stack or frame list was shortened.

## 0.1.0

- Add routed, read-only `module.resources` and bounded `module.profile` queries for
  direct operators and owner-authorized flows. Profiles summarize macOS stacks or
  Linux thread CPU, with process-generation checks and terminal refusals.
