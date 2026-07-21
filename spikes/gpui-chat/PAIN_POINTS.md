# GPUI SubcChat field log

Evaluation date: 2026-07-21

Toolchain: `rustc 1.97.0 (2d8144b78 2026-07-07)`

Pin: `gpui = 0.2.2` exactly, with `font-kit` and `runtime_shaders`

This log distinguishes framework capability from documentation/package friction. Times are hands-on investigation and adaptation time, rounded to the nearest five minutes.

## Framework and packaging

### The requested `gpui_platform = "0.2"` package does not exist on crates.io

- **Trying to do:** Follow the standalone setup in the spike brief: `gpui = "0.2"` plus `gpui_platform = "0.2"` with `font-kit`.
- **What GPUI offered:** crates.io has `gpui 0.2.2`, but a search and resolver probe found no official `gpui_platform` package at any 0.2 version. The published `gpui` crate now exports `Application` and selects the macOS backend itself.
- **Severity:** friction
- **Time lost:** 20 minutes
- **Workaround:** Depend only on exact `gpui 0.2.2`, enable its `font-kit` feature, and start with `gpui::Application::new()`.
- **Diagnosis:** stale packaging documentation/task guidance, not missing runtime capability.

### Metal compilation assumes full Xcode

- **Trying to do:** Compile the stock crate on a normal Command Line Tools-only development machine.
- **What GPUI offered:** the default build script shells out to `xcrun metal`; CLT does not ship that utility, so the build failed before application code was checked.
- **Severity:** friction
- **Time lost:** 15 minutes
- **Workaround:** enable GPUI's `runtime_shaders` feature. The app then compiled and asks Metal to compile source at runtime.
- **Diagnosis:** packaging/default-feature problem. The capability exists but the useful fallback is not surfaced by the primary examples.

### Local source examples and crates.io 0.2.2 have API drift

- **Trying to do:** Learn from the explicitly recommended Zed checkout examples (`hello_world`, `input`, `uniform_list`, `gradient`, and `shadow`).
- **What GPUI offered:** excellent concrete examples, but the checkout's newer APIs differed from 0.2.2: `gpui_platform::application` vs `Application`, `BoxShadow::new` vs struct/preset shadows, `Context::processor` vs a plain `uniform_list` closure, `on_click` availability, and text paint arguments.
- **Severity:** friction
- **Time lost:** 45 minutes
- **Workaround:** inspect the exact registry source for 0.2.2 and adapt each call.
- **Diagnosis:** missing versioned docs/examples. Compiler diagnostics were good enough to recover, but this is expensive onboarding.

### First build is very large

- **Trying to do:** get a small native spike to first pixels.
- **What GPUI offered:** a broad rendering/media/platform stack; Cargo resolved about 700 packages and the initial test build took materially longer than app iteration.
- **Severity:** papercut
- **Time lost:** 10 minutes of wall-clock wait
- **Workaround:** subsequent incremental checks are fast (about one second here).
- **Diagnosis:** framework architecture/feature granularity rather than docs.

## Layout and visual polish

### Styling polished cards is pleasantly direct

- **Trying to do:** create a dark product identity rather than an inspector/debug-tool look.
- **What GPUI offered:** fluent flexbox, spacing scale, rounded corners, borders, opacity colors, preset shadows, gradients, hover/active states, and typography in one element chain.
- **Severity:** pleasant surprise
- **Time saved:** approximately 60 minutes compared with writing AppKit wrappers; comparable to SwiftUI for these primitives.
- **Evidence:** sidebar gradient mark, luminous source/status treatment, layered project/agent cards, pills, progress strips, selected master rows, and gradient submit action are all stock GPUI.
- **Diagnosis:** strong framework capability despite sparse docs.

### Interactive elements change the concrete Rust return type

- **Trying to do:** extract reusable helpers such as navigation rows and detail panes.
- **What GPUI offered:** adding `.id()` or an event handler wraps `Div` in `Stateful<Div>`, so helpers declared to return `Div` stop compiling and conditional branches frequently have incompatible types.
- **Severity:** friction
- **Time lost:** 30 minutes
- **Workaround:** return `AnyElement` at surface/component boundaries and keep leaf-only visual helpers as `Div`.
- **Diagnosis:** inherent static-builder ergonomics. Better cookbook guidance could reduce the surprise.

### Scroll behavior depends on stateful identity

- **Trying to do:** make independently scrolling master/detail panes.
- **What GPUI offered:** `overflow_y_scroll` is available only through `StatefulInteractiveElement`; a plain `Div` must gain an id first. The error only says the method is missing.
- **Severity:** papercut
- **Time lost:** 10 minutes
- **Workaround:** assign stable ids to every scroll region.
- **Diagnosis:** mostly discoverability/documentation.

### Virtualized fixed-height lists are easy

- **Trying to do:** prevent pending asks and consult history from growing the render tree.
- **What GPUI offered:** `uniform_list` is small, fast, and straightforward once using the exact-version closure signature.
- **Severity:** pleasant surprise
- **Time saved:** about 20 minutes versus manual AppKit collection view plumbing.
- **Caveat:** it assumes uniform heights, so variable-height project sections and board lanes still use scroll containers. A production component layer needs a strategy for heterogeneous virtualized rows.

### Animation was not product-ready in the time box

- **Trying to do:** add cross-surface and progress transitions with `with_animation`.
- **What GPUI offered:** lower-level animation primitives and examples, but no obvious stock transition/navigation abstraction.
- **Severity:** friction
- **Time lost:** 15 minutes of source reading; implementation was dropped in favor of responsive hover/active feedback.
- **Workaround:** immediate state changes plus hover, active opacity, gradients, and shadows.
- **Diagnosis:** component/ecosystem gap more than renderer limitation.

## Text input: headline finding

### Stock GPUI has an input protocol, not a text field

- **Trying to do:** provide a trustworthy answer composer with Unicode, IME, selection, clipboard, and multiple lines.
- **What GPUI offered:** `EntityInputHandler`, focus/key routing, shaped text, clipboard APIs, and low-level painting. The stock `examples/input.rs` is effectively the reference implementation and is hundreds of lines, not a reusable control.
- **Severity:** **blocker for shipping without an owned component layer**
- **Time lost:** about 2 hours for a deliberately reduced composer
- **Workaround:** port the pattern from GPUI's own `examples/input.rs`: UTF-8/UTF-16 conversion, marked ranges, `ElementInputHandler`, custom cursor/selection paint, grapheme movement, and explicit key bindings. No code was taken from `gpui-component`; reviewing its `InputState` (its `EntityInputHandler` implementation is around line 2816) confirmed that a production implementation is correspondingly large rather than revealing a small hidden primitive.
- **Diagnosis:** missing stock framework capability/component, not just missing docs.

### Honest behavior matrix for this spike's composer

| Behavior | Result |
|---|---|
| Basic insertion/deletion | Implemented |
| Unicode grapheme left/right | Implemented |
| Marked text / IME protocol | Implemented through `replace_and_mark_text_in_range`; visually underlined |
| Copy/cut/paste | Implemented with explicit key bindings |
| Select all and shift-arrow selection | Implemented |
| Mouse drag selection / precise hit testing | Not implemented |
| Undo/redo | **Not implemented** |
| Multiline storage | Implemented; Return inserts `\n` |
| Multiline layout/caret | **Not implemented**; compact renderer shows line breaks as `↵` on one shaped line |
| Accessibility semantics | Not evaluated |

Copy/paste and direct option-fill were exercised in code. Full IME composition, accessibility, VoiceOver, undo grouping, and multiline caret navigation require hands-on QA beyond automated tests. The app does not claim otherwise.

## Data and concurrency

### Native Rust wire access is materially cleaner than FFI

- **Trying to do:** authenticate and call `alfonso-core` without blocking rendering.
- **What GPUI offered:** the background executor composes cleanly with the repository's `SubcConsumer`; no JSON bridge, Swift wrapper, or FFI layer was needed.
- **Severity:** pleasant surprise
- **Time saved:** likely days compared with maintaining equivalent bindings.
- **Implementation evidence:** calls use the path dependency, `ManagementSurface` target, `ck-app` route identity, unique `gpui-spike-<uuid>` session, and exact `{method, params}` / `{result}` envelopes.

### Two async runtimes need an explicit bridge

- **Trying to do:** run Tokio-based socket work under GPUI's executor.
- **What GPUI offered:** GPUI's executor is runtime-agnostic, while `SubcConsumer` expects Tokio I/O context.
- **Severity:** friction
- **Time lost:** 20 minutes
- **Workaround:** every socket operation is spawned on `cx.background_executor()` and creates a small two-worker Tokio runtime there. A 12-second outer deadline and detached runtime shutdown contain client/provider stalls. The UI thread never performs connection, authentication, decoding, or calls.
- **Diagnosis:** normal ecosystem composition, but worth standardizing in a real app to avoid per-refresh runtime creation.

### Publish-only-on-change is explicit and understandable

- **Trying to do:** poll board data every 2.5 seconds without SwiftUI's prior unconditional graph diff problem.
- **What GPUI offered:** ordinary Rust equality and explicit `cx.notify()`.
- **Severity:** pleasant surprise
- **Workaround:** decoded snapshots derive `PartialEq`; state is replaced and GPUI notified only when payload/source/error changes. An in-flight guard prevents overlapping polls. Polling is active only on the Boards surface.
- **Diagnosis:** GPUI's explicit invalidation model is an advantage for this workload.

### Live daemon result in this environment

The app attempts live access first on every launch. A read-only `cargo run -- --probe-live` reached and authenticated to the local daemon, then received `unknown_method` for `board.list` from the deployed `alfonso-core`. The app therefore uses the bundled canonical `board-wire-fixtures-v1.json` and `spec-status-wire-fixtures-v1.json` fallback in this environment. The source badge clearly labels the mode. No `ask.persist_answer` call was made during development or tests.

## Models and testing

### Tolerant serde models are simpler than the Swift normalizer

- **Trying to do:** preserve optional-when-absent semantics, ignore additive fields, and accept snake/camel drift.
- **What GPUI/Rust offered:** serde ignores unknown fields by default; `#[serde(default, rename_all = "camelCase")]` plus aliases covers the wire tolerance with little code.
- **Severity:** pleasant surprise
- **Time saved:** about 30 minutes versus recursive Foundation object normalization.

### The canonical board fixture contains references, not expanded blocks

- **Trying to do:** render `boardState` directly.
- **What was offered:** its `blocks` array uses `$ref` documentation placeholders while concrete block cases live in the same file's top-level `blocks` array. One legal work block also uses a timestamp-sized revision, disproving an initial `i32` assumption.
- **Severity:** papercut
- **Time lost:** 15 minutes
- **Workaround:** fixture loading injects the concrete block cases and models revisions as `i64`; live responses decode normally.
- **Diagnosis:** fixture/document format, not GPUI.

## Verdict

**Recommendation: conditional yes for a Rust-first product, but not yet on “stock GPUI only.”**

GPUI convincingly clears the visual-polish concern. Cards, hierarchy, gradients, hover states, status color, dense split surfaces, and virtualized lists were pleasant to build and can look like a product rather than an editor. Its explicit invalidation and direct Rust client integration are particularly compelling for SubcChat's polling workload and eliminate an entire FFI/wire duplication layer.

The condition is a funded component layer before product work begins. Stock GPUI is a renderer/application framework, not a macOS controls toolkit. Shipping asks, settings, forms, menus, accessibility, and rich text against only stock primitives would move substantial platform work onto the product team. The longbridge library demonstrates that the gap is closable, but its very large input implementation also demonstrates the cost.

### Top three risks

1. **Text editing and accessibility ownership.** A production multiline editor with IME, undo grouping, mouse selection, bidi, VoiceOver, and validation is far beyond this spike's reduced composer.
2. **Versioned documentation/package churn.** The official package instruction was unresolvable and recommended source examples had several API generations of drift from the pinned crate.
3. **Component and navigation ecosystem gap.** Polished primitives are easy, but dialogs, menus, focus traversal, heterogeneous virtual lists, transitions, forms, and system-consistent controls need an internal or third-party layer.

### What this spike could not evaluate

- VoiceOver/accessibility tree quality and full keyboard traversal
- sustained CJK/complex-script IME behavior, bidi text, dictation, and emoji palette edge cases
- undo/redo grouping and production multiline editing
- light/system-following theme and reduced-motion behavior
- window restoration, native menus, sheets/modals, drag-and-drop, and notifications
- large heterogeneous board virtualization and hours-long memory/GPU behavior
- signed/notarized distribution, binary size, cold-start time, and runtime shader policy
- live fleet behavior under daemon restart, route restoration, and high-volume poll churn
