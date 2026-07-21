# SubcChat GPUI evaluation spike

A stock-GPUI macOS clone of the three decision surfaces in `SubcChat`: project boards, pending asks, and Athena consult/spec ladders.

## Run

```sh
cd spikes/gpui-chat
cargo run
```

The app first renders the bundled canonical wire fixtures, then attempts a managed `SubcConsumer` connection through `~/.local/share/cortexkit/run/subc-connection.json`. The source badge in the lower-left corner always says **LIVE** or **FIXTURE**. Fixture mode is read-only. In live mode only the explicit **Send answer** button invokes `ask.persist_answer`; tests never perform mutations.

GPUI 0.2.2 compiles Metal shaders at runtime so a Command Line Tools-only macOS installation can build the spike. The first build is large (GPUI's rendering/image stack is roughly 700 resolved packages).

## Interaction notes

- Boards refresh every 2.5 seconds only while the Boards surface is selected. Socket and decoding work runs on GPUI's background executor, with a small bounded Tokio runtime for the asynchronous Rust client. Snapshots replace UI state only when data changes.
- Ask and consult master lists use `uniform_list` virtualization.
- The answer composer implements GPUI's `EntityInputHandler`, including marked-text/IME ranges, UTF-8 ↔ UTF-16 conversion, selection actions, and clipboard actions. Return stores a line break; the compact visual editor shows it as `↵` on one shaped line. Undo/redo is not implemented; see `PAIN_POINTS.md`.
- Clicking an option copies its exact label into the composer. Sending in fixture mode is intentionally refused.

## Verification

```sh
cargo test
cargo clippy --all-targets
cargo check
# Optional non-UI connectivity diagnostic (read-only):
cargo run -- --probe-live
```

The bundled tests decode both canonical fixture documents, fold newest board revisions, verify snake-case tolerance, and exercise the Swift `ProjectGrouping` semantics.
