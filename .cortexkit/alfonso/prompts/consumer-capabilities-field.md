# Add typed `consumer_capabilities` to route.open / RouteBind (protocol 0.8 content)

Repo: this workspace (subconscious). Branch from master.

## Why

The subc-mcp facade relays reverse requests (elicitation/create, sampling/createMessage, roots/list) from provider modules to MCP hosts. Provider modules (AFT) need to know AT BIND TIME whether the consumer behind a route can answer a given reverse-request class, so they can fail closed immediately (flat deny) instead of sending asks that hang until TTL. Today nothing on the RouteBind wire carries this; AFT probes bind metadata defensively and finds nothing.

## Wire change (additive, optional, deserialize-tolerant)

1. `crates/subc-control/src/lib.rs` — `ClientControlRequest::RouteOpen` gains:
   `#[serde(default, skip_serializing_if = "Option::is_none")] consumer_capabilities: Option<Vec<String>>`
2. `crates/subc-protocol/src/session.rs` — `ModuleControlRequest::RouteBind` gains the same field, same serde attributes.
3. `crates/subc-core/src/control.rs` — `handle_route_open` copies the field VERBATIM from RouteOpen onto the RouteBind relay. The daemon does not validate, interpret, or filter the list (thin-core: opaque declaration relay, same posture as consumer_identity pass-through after verification). Absent stays absent (None must not serialize as `[]`).
4. `crates/subc-mcp/src/main.rs` — when opening a provider route for a shim session, stamp the field from the session's captured `ReverseCapabilities` (the struct at ~line 111): include `"elicitation"`, `"sampling"`, `"roots"` for each capability the MCP host advertised at initialize. If the host advertised none, send None (not an empty vec). `open_provider_route` (~line 1742) needs the capabilities plumbed in from the relay session that triggers the open — follow the existing call path from the shim-session attach to open_provider_route and thread it through.
5. Clients:
   - `clients/subc-client/src/client.ts` — `RouteOpenOptions` gains optional `consumerCapabilities?: string[]`; `routeOpen` includes it in the route.open body as `consumer_capabilities` when set. Provider side: `RouteBindRequest` interface gains optional `consumer_capabilities?: string[]` (read-only exposure to serve handlers).
   - `crates/subc-client-rs` — consumer `route.open` path gains the same optional field on its open options (default None); module-serve side exposes the field on the bind request struct handed to `on_bind`.
   - Swift client: add the optional field to the route.open encoder ONLY if trivially reachable in Client.swift; otherwise leave Swift untouched and note it (Swift chat doesn't use reverse requests).

## Semantics to document (doc comments on the fields)

- This is a capability DECLARATION by the consumer, not a verified privilege. A consumer that over-declares receives reverse requests it cannot answer; provider-side TTL settles them as deny. Providers must treat absent/None as "no reverse-request capability" (fail closed).
- Vocabulary is open strings; current known values: "elicitation", "sampling", "roots" (MCP reverse-request method families).

## Tests

- Golden JSON vectors: update the control-protocol golden files for RouteOpen/RouteBind BOTH with and without the field (absent field must round-trip as absent — deserialize-tolerance both directions).
- subc-core relay test: route.open with consumer_capabilities → module receives RouteBind carrying the identical list; route.open without → RouteBind has None.
- subc-mcp test: a shim session whose host advertised elicitation at initialize produces a provider route.open stamped ["elicitation"]; a host advertising nothing produces None. Extend the existing facade tests rather than building a new rig if possible.
- TS client test: routeOpen with consumerCapabilities includes the snake_case field on the wire; without it, the key is absent from the JSON body.

## Gates

cargo fmt, clippy clean (also x86_64-pc-windows-gnu per repo rule), full test suite, TS client `bun test` (unit-only, no RUN_SUBC_LIVE needed). Commit with a message explaining the bind-time capability declaration. Do NOT bump crate versions or publish — version bump and the paired crates.io release happen outside this task.
