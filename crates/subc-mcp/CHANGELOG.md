# Changelog

## 0.1.24

- Updates the daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. The gateway sends no preset because its MCP host supplies none; providers must decide explicitly what an absent preset gets.

## 0.1.23

- Preserve outcome-unknown metadata when a provider closes a dispatched request.
- Keep attached sessions alive across transient shim accept failures.
- Release completed prompt relay routes and omit combined tool names exceeding 64 characters.

## 0.1.19 — 2026-10-01

- Takes `subc-protocol` 0.28.0 and `subc-daemon` 0.27.0. The gateway sends no `origin` on the tool calls it routes, because those calls come from the host it serves rather than being relayed for another caller, and it declares no `role_versions` on the routes it opens. No behavior change.
