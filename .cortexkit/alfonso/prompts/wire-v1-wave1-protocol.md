# Wire v1-final — Wave 1: wire-shape crates (subc-protocol, subc-transport, subc-control)

You are implementing the FOUNDATION of a frozen, Oracle-gated wire revision. The authoritative spec is `docs/specs/subc-wire-v1-final.md` (v7, commit 5a582f2b) — READ IT FIRST, especially §1 (layout), §2 (ver/prefix-first), §3.2/§3.3.1 (control-op shapes), §5 (flags), §6 (decode taxonomy), §10 (test plan). Do not deviate from the spec; if you believe the spec is wrong somewhere, STOP and ask rather than improvising.

This is an IN-PLACE revision: no version negotiation, no dual codec, no backward compatibility. The old 17-byte layout dies; every test/fixture moves to the new one. Later waves (daemon, clients, subc-mcp) build on your output — your job is ONLY the three wire-shape crates listed below. Do NOT touch subc-core, subc-mcp, or the clients beyond what compiles (see "downstream breakage" below).

## Scope

### 1. crates/subc-protocol
- `EnvelopeHeader`: 21 bytes LE — len u32 [0..4] · ver u8 [4] · ty u8 [5] · flags u8 [6] · channel u16 [7..9] · **epoch u32 [9..13] (NEW)** · corr u64 [13..21]. `HEADER_LEN` 17→21. `FROZEN_PREFIX_LEN` stays 5.
- `PROTOCOL_VERSION` 1→2, `MIN_SUPPORTED_VERSION` 1→2. Exactly one supported version.
- Flags byte: bit0 BINARY, bits1-2 PRIORITY (unchanged, 0b11 rejected), bit3 LAST, **bits4-5 ADMISSION CLASS (NEW)**: 00 NORMAL, 01 EXPEDITE, 10 SHEDDABLE, 11 decode-rejected. Bits6-7 reserved-must-be-zero (update `FLAG_RESERVED_MASK` from bits4-7 to bits6-7). Add a typed `AdmissionClass` enum + accessors mirroring the `Priority` pattern; extend `Flags::new` (or add a builder) so callers can set the class; default NORMAL.
- SHEDDABLE legality: class 10 is legal ONLY on `Push` and `StreamData` — enforce at decode.
- Channel-0 epoch rule: `channel == 0 && epoch != 0` is a decode error.
- `DecodeError` additions per spec §6: `ReservedAdmissionClass { flags }`, `SheddableIllegalFrameType { ty, flags }`, `NonzeroEpochOnControlChannel { epoch }`; `TooShortForHeader.need` becomes 21. Display impls for each. `ReservedFlagBits` now fires on bits6-7 only.
- `Frame` builders (`Frame::build`, `build_with_version`, etc.): add the epoch parameter. Every construction site inside this crate updated.
- `session.rs`: `ModuleControlRequest::RouteBind` gains `epoch: u32`; `ModuleControlPush::RouteStatus` gains `route_epoch: u32`. Plain required fields (in-place revision — NOT serde(default)/optional; the whole fleet rebuilds together).
- Golden JSON fixtures in this crate: regenerate for the changed shapes. Envelope byte-layout tests: rewrite for 21 bytes, covering epoch boundaries (0, 1, u32::MAX) and all four admission-class values (three legal round-trips + class-11 rejection), SHEDDABLE-on-illegal-type rejection per frame type, nonzero-epoch-on-channel-0 rejection. All rejection tests must assert the EXACT DecodeError variant (non-vacuous).

### 2. crates/subc-transport
- `frame_io.rs` `read_frame`: PREFIX-FIRST reads (spec §2) — read exactly the 5-byte frozen prefix, validate `ver == 2` (return the unsupported-version error BEFORE attempting to read more), then read the remaining `HEADER_LEN - 5` header bytes, then the body. PRESERVE the existing body-size rejection before allocating `len` bytes. This ordering is normative: a stale 17-byte-sender's pure-header frame must produce UnsupportedVersion promptly, never block waiting for bytes that will not arrive. Add exactly that test: feed a v1-shaped 17-byte pure-header frame into the reader and assert prompt UnsupportedVersion{ver:1} (use a duplex/mock stream; must not hang).
- `write_frame` and any header-size assumptions: update to 21.
- Auth handshake is PRE-envelope and unchanged — do not touch auth.rs beyond what compiles.

### 3. crates/subc-control
- `ClientControlResponse::RouteOpen` gains `route_epoch: u32`.
- `ClientControlRequest::RoutePoll` gains `route_epoch: u32`.
- RoutePoll RESPONSE arm(s): echo `route_channel: u16` and `route_epoch: u32` in EVERY arm including unknown-route (spec §3.3.1; absent/stale answers: status query → status:null/live:null, liveness → status:null/live:false).
- Golden JSON fixtures regenerated for changed shapes.

## Downstream breakage policy
subc-core, subc-mcp, subc-client-rs, and the stub WILL break against your changes — that is EXPECTED and later waves own the real fixes. But per the never-leave-red rule, make the WORKSPACE compile: mechanical stopgap edits only (pass epoch 0 / thread the new field with a placeholder from the binding where one obviously exists, add the new struct fields at construction sites with obvious values). Mark every such stopgap with a `// WIRE-WAVE2:` comment so wave 2 can grep them. Do NOT implement daemon epoch logic (minting, validation, fencing) — that is wave 2's job and doing it here creates merge conflicts. Keep stopgaps minimal and mechanical.

## Verification bar (all required before you commit)
1. `cargo test --workspace` green.
2. `cargo clippy --workspace --all-targets` clean AND `cargo clippy -p subc-protocol -p subc-transport -p subc-control --target x86_64-pc-windows-gnu --all-targets` clean (Windows cfg gaps are a recurring CI killer).
3. `cargo fmt --all`.
4. The new decode-rejection tests are non-vacuous (assert exact variants).
5. The prefix-first stale-frame test proves promptness (bounded, not hanging).

Commit with a clear message. Report: what changed per crate, the stopgap count + locations, and test totals.
