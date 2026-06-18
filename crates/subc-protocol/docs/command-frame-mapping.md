# Command → frame-shape mapping

This is the Story 1.1 AC3 representative mapping for the local
`subc ↔ module` leg. It deliberately covers frame-shape classes rather than
enumerating every AFT command, preserving the envelope extensibility described
in `docs/subc-core-architecture.md` §4.8.

Architecture §4.8 fixes the 17-byte little-endian header:
`len u32`, `ver u8`, `type u8`, `flags u8`, `channel u16`, `corr u64`, followed
by `len` body bytes. `flags` bit 0 is `BINARY`, bits 1-2 are priority
(`passive`, `interactive`, `background`), bit 3 is `LAST`, and bits 4-7 are
reserved. `CANCEL` is a pure-header frame (`len = 0`) whose `corr` is the target
call. Large payloads stream; raw vector payloads use the bulk lane (`BINARY`)
instead of trying to fit an unbounded JSON body under the `len: u32` frame cap.

## Mapping table

| Command / operation | Concrete frame sequence | Flags | Streams? | PUSH emitted? | Notes |
| --- | --- | --- | --- | --- | --- |
| `read` (normal, within the 50KB JSON result cap) | `REQUEST` → `RESPONSE` on the same `corr` | `REQUEST`: `interactive`; `RESPONSE`: `interactive`; `BINARY=0`, `LAST=0` | No | No | JSON-RPC request/result bodies travel in the normal JSON path. |
| `outline` | `REQUEST` → `RESPONSE` on the same `corr` | `interactive`; `BINARY=0`, `LAST=0` | No | No | Same single-call shape as normal `read`. |
| `status` | `REQUEST` → `RESPONSE` on the same `corr` | `passive` for polling/status traffic; `BINARY=0`, `LAST=0` | No | No | Liveness can be answered by subc, while module-originated state is returned as JSON. |
| Large `read` result past the 50KB cap | `REQUEST` → `STREAM_DATA` × N → `STREAM_END` on the same `corr` | `REQUEST`: `interactive`; each `STREAM_DATA`: `interactive`, `BINARY=0`, `LAST=0`; `STREAM_END`: `interactive`, `BINARY=0`, `LAST=1` | Yes | No | Chunks carry JSON/text payload pieces; the final `STREAM_END` closes the stream. This is still the JSON/text path, not bulk binary. |
| Large `callgraph` dump | `REQUEST` → `STREAM_DATA` × N → `STREAM_END` on the same `corr` | `REQUEST`: `background` or `interactive` by caller intent; stream frames keep that priority; `BINARY=0`; `LAST=1` only on `STREAM_END` | Yes | No | Uses streaming when the graph is too large for one JSON response frame. |
| Embedding vectors (`embed` / `upsert_vectors`, internal AFT ↔ embedding service) | JSON control `REQUEST` → binary `STREAM_DATA` × N → binary `STREAM_END` | control `REQUEST`: `background`, `BINARY=0`; vector frames: `background`, `BINARY=1`; `LAST=1` only on `STREAM_END` | Yes | No | Bulk-lane payloads are raw vector bytes. Metadata such as model, dimensions, and ids stays in JSON control/trailer data; the raw vector body is not JSON and is bounded per frame by `len: u32`. |
| `bash` | `REQUEST` → immediate `RESPONSE` to acknowledge task start, then async `PUSH` frames for output/completion events | `REQUEST`/start `RESPONSE`: `interactive`, `BINARY=0`; output `PUSH`: usually `interactive`, `BINARY=0`, `LAST=0`; completion `PUSH`: `interactive`, `BINARY=0`, `LAST=1` when it is the final event for that task | Output may be chunked as events, but not as the request response stream | Yes | The `RESPONSE` returns the task handle/start result. Later `PUSH` bodies carry task output and terminal status keyed by task id. |
| `semantic_search` cold-build canceled mid-flight | `REQUEST` with `corr = C`; later `CANCEL` with `corr = C` and `len = 0` | `REQUEST`: `interactive` or `background` by caller intent, `BINARY=0`; `CANCEL`: same priority class is acceptable, `BINARY=0`, `LAST=0`, `len=0` | No, unless the implementation had already chosen a stream result | No | The terminal result is race-dependent: the module may observe cancellation and stop, or the response may already have been produced. The frame-shape requirement is the pure-header `CANCEL` targeting the original correlation id. |
| `callgraph` cold-build canceled mid-flight | `REQUEST` with `corr = C`; optional early `STREAM_DATA`; `CANCEL` with `corr = C` and `len = 0`; no further stream frames after cancellation is observed | `REQUEST`/stream priority by caller intent; stream frames `BINARY=0`; `STREAM_END` has `LAST=1` only if the stream completes before cancel; `CANCEL` is pure-header | May have started streaming before cancel | No | Exercises the same cancel delivery contract for a long project-scoped build/dump. |
| Subc-generated routing/control error | `ERROR` on the relevant `channel`/`corr` | `passive`; `BINARY=0`, `LAST=0` | No | No | Body is canonical JSON `ErrorBody { code: string, message: string }` for all subc-generated errors, including control-plane errors and unknown-channel routing failures. |

## Frame-shape classes covered

1. **Single request/response:** `read`, `outline`, `status`.
2. **Text/JSON streaming:** large `read` and large `callgraph` use
   `STREAM_DATA` frames followed by `STREAM_END` with `LAST`.
3. **Bulk lane:** embedding vector traffic sets `BINARY` on vector payload
   frames and keeps JSON metadata out-of-band in control/trailer data.
4. **Async push:** `bash` starts with a normal response and emits later `PUSH`
   frames for output and completion.
5. **Cancellation:** `semantic_search` and `callgraph` cold-builds are interrupted
   by pure-header `CANCEL` frames whose `corr` targets the in-flight request.
6. **Subc-generated errors:** `ERROR` frames carry canonical JSON
   `ErrorBody { code, message }` regardless of whether they originate in the
   control path or the router's unknown-channel path.
