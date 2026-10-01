# Changelog

## 0.1.19 — 2026-10-01

- Takes `subc-protocol` 0.28.0 and `subc-daemon` 0.27.0. The gateway sends no `origin` on the tool calls it routes: it is not relaying for another caller, the host it serves is the caller. No behavior change.
