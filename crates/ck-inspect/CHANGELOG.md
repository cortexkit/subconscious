# Changelog

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
