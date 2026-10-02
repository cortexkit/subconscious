# Changelog

## 0.27.1

- Correct the restart drain override documentation: older daemons ignore the unknown field and use their normal drain budget, rather than refusing the request. No wire or runtime change.

## 0.27.0 — 2026-10-02

- Breaking Rust API: `RouteClosing` and `RouteClosed` gain `channels: Vec<u16>`, naming the receiving connection's affected routes. Struct literals and exhaustive patterns must include the field. Older wire pushes decode with an empty list; current serialization always emits the `channels` field.

## 0.26.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet.
- Breaking: `ClientControlRequest::RouteOpen` gains `role_versions: Option<BTreeMap<String, String>>`, the provider-role versions the consumer speaks on the route (`{"tool-provider": "v1"}`). It is omitted on the wire when `None`, and an empty map means the same as `None`. Send it only to a daemon advertising `route-role-versions/v1`. New golden vectors: route.open with and without it.
