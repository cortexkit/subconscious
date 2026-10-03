# Changelog

## 0.1.5

- Give module cgroup directories an injective byte encoding and a dedicated `m-` namespace, preventing aliases and collisions with kernel interface files. Existing unprefixed directories are not reused; restart supervised processes when upgrading.

## 0.1.4 — 2026-10-02

- Added the safe `kill_module` API and typed `KillOutcome` for atomic cgroup v2 subtree termination, distinguishing a module with no cgroup placement, unavailable kernel support, and I/O failures that include the affected path.
