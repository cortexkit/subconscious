# Changelog

## 0.1.6

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.5

- Give module cgroup directories an injective byte encoding and a dedicated `m-` namespace, preventing aliases and collisions with kernel interface files. Existing unprefixed directories are not reused; restart supervised processes when upgrading.

## 0.1.4 — 2026-10-02

- Added the safe `kill_module` API and typed `KillOutcome` for atomic cgroup v2 subtree termination, distinguishing a module with no cgroup placement, unavailable kernel support, and I/O failures that include the affected path.
