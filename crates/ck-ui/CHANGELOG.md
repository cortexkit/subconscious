# Changelog

## 0.2.1

- No Mac-only wording: every screen says "this computer", and the pairing
  flow says "Waiting for confirmation on this computer" instead of naming
  Touch ID. The top bar's default label is "This computer" on every OS.
- Read Callosum's `computer` device class, and keep reading the older `mac`
  spelling; both show as "computer".

## 0.2.0

- Minor bump because this crate's public API carries subc-control types (`live::map::route_consumers` takes a `SupervisorRouteModule`), and subc-control moves to 0.33 with subc-protocol 0.33.0. Move to subc-protocol 0.33, subc-control 0.33, subc-client-rs 0.31 and subc-transport 0.14 together. No behavior change.
