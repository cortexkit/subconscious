# Changelog

## 0.1.19 — 2026-10-01

- Takes `subc-protocol` 0.28.0 and `subc-daemon` 0.27.0. The gateway sends no `origin` on the tool calls it routes, because those calls come from the host it serves rather than being relayed for another caller, and it declares no `role_versions` on the routes it opens. No behavior change.
