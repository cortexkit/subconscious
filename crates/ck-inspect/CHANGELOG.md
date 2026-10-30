# Changelog

## 0.1.6

- Name stripped macOS profile frames using a module's UUID-indexed dSYM bundles. Symbolization shares the sampling deadline, is bounded to 256 addresses per image, and preserves image offsets when symbols are unavailable. Merge named stacks before ranking and applying reply limits.
- Treat blocking socket reads, accepts, connects, select/pselect, and waitid calls as idle leaves so blocked threads do not outrank running code.

## 0.1.5

- macOS profiles rank threads by `busy_samples`, the samples whose leaf frame was not a known wait, and list only those running stacks. `sample` samples every thread at every tick, so 0.1.4 ranked by equal totals and then by name, and idle threads filled the eight-thread and 16 KiB limits before the busy ones.
- More kernel waits count as idle: mutex and `__ulock` waits, `kevent64`, `mach_msg2_trap`, semaphore traps, blocking `read`, `select`, `poll`, `sigsuspend` and `wait4`.
- A frame in a stripped binary is reported as `image+0xOFFSET` instead of `???`, so its owner can symbolize it against that build's debug symbols.

## 0.1.4

- Wrap successful replies in the standard management envelope, `{"result": <value>}`. 0.1.3 returned the bare value, which management callers such as flows through Basal refuse as unreadable. Refusals still go out as Error frames. The profile is trimmed so the whole reply, envelope included, stays within 16 KiB.

## 0.1.3

- Read requests in the fleet's management envelope, `{"method": "<operation>", "params": {...}}`, which flows through Basal, the phone and other management callers send. 0.1.2 and earlier expected `{"op": ...}` with fields at the top level, so every routed caller except `ck` was refused. `params` may be omitted for `module.resources`. `ck module resources` and `ck module profile` now send the same envelope.

## 0.1.2

- `module.profile` accepts `seconds` sent as a whole-valued float such as `8.0`, which callers in JavaScript (flows through Basal) can send for 8. Fractions, strings and values outside 1 to 10 are still refused.
- A refused request now names what was wrong: missing `op`, an unknown op, an unexpected field, `module_id` not a string, or `seconds` out of range. The reasons are fixed text and never echo the caller's input.

## 0.1.1

- A profile no longer reports `truncated: true` because an idle thread's deep wait stack was cut. Idle threads are dropped from the reply whole, so cutting their stacks removes nothing the reply would have shown. `truncated` now means a reported thread, stack or frame list was shortened.

## 0.1.0

- Add routed, read-only `module.resources` and bounded `module.profile` queries for
  direct operators and owner-authorized flows. Profiles summarize macOS stacks or
  Linux thread CPU, with process-generation checks and terminal refusals.
