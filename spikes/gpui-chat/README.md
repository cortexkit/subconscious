# SubcChat GPUI evaluation spike

A stock-GPUI macOS port of the daily broca chat, multi-agent rooms, project boards, pending asks, and Athena consult/spec-ladder surfaces from `SubcChat`.

## Run

```sh
cd spikes/gpui-chat
cargo run
```

The app first renders the bundled canonical wire fixtures, then attempts a managed `SubcConsumer` connection through `~/.local/share/cortexkit/run/subc-connection.json`. The source badge in the lower-left corner always says **LIVE** or **FIXTURE**. Fixture mode is read-only. In live mode only the explicit **Send answer** button invokes `ask.persist_answer`; tests never perform mutations.

GPUI 0.2.2 compiles Metal shaders at runtime so a Command Line Tools-only macOS installation can build the spike. The first build is large (GPUI's rendering/image stack is roughly 700 resolved packages).

## Interaction notes

- Chat sessions persist locally and stream broca display/control events through `SubcConsumer::subscribe`. Command and subscription routes use separate managed consumers so a held-open stream never blocks `session.send`. Tool calls, tool results, typed provider errors, non-completed finish reasons, stale cursors, and mid-turn route teardown are rendered explicitly.
- Chat offers the Swift client's verified model presets, an editable `provider/model` field, and the live aft tool catalog, plus a native directory picker for each session's project root. The root locks after the first message because it is part of the module bind identity.
- Pending asks poll every 5 seconds for the app's whole lifetime. The first backlog is quiet, while later arrivals update the dock badge and trigger an urgency-aware AppKit bounce/sound plus a background `osascript` banner.
- Rooms polls the open transcript every 2.5 seconds and the room list every 10 seconds while visible. It supports invitations, enter/leave, UUID-deduplicated posts and signals, automatic ACKs, transcript replay, and the member reaction/stage board strip.
- Boards refresh every 2.5 seconds only while the Boards surface is selected. Socket and decoding work runs on GPUI's background executor, with a small bounded Tokio runtime for the asynchronous Rust client. Snapshots replace UI state only when data changes.
- Observe polls Athena consults/spec campaigns plus gather and comment-check runs every 2.5 seconds while visible. Consult details include per-stage usage and server-computed per-model rollups; every surfaced run can open a paginated, deduplicated broca transcript with collapsed system prompts and lineage errors. Lists and variable-height transcript rows are virtualized.
- The shared composer implements GPUI's `EntityInputHandler` with marked-text/IME ranges, UTF-8 ↔ UTF-16 conversion, soft-wrapped multiline shaping, vertical caret navigation, precise mouse drag selection, clipboard actions, and bounded grouped undo/redo. Remaining complex-script, bidi, and accessibility QA is tracked in `PAIN_POINTS.md`.
- Clicking an option copies its exact label into the composer. Sending in fixture mode is intentionally refused.

## Verification

```sh
cargo test
cargo clippy --all-targets
cargo check
# Optional non-UI connectivity diagnostics (read-only):
cargo run -- --probe-chat
cargo run -- --probe-rooms
cargo run -- --probe-observe
cargo run -- --probe-live
```

The bundled tests decode canonical fixtures, fold newest board revisions, verify tolerant room/observe wire casing and transcript blocks, exercise grouped editor history, and preserve the Swift `ProjectGrouping` semantics.
