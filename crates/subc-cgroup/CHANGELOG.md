# Changelog

## 0.1.4 — 2026-10-02

- Added the safe `kill_module` API and typed `KillOutcome` for atomic cgroup v2 subtree termination, distinguishing a module with no cgroup placement, unavailable kernel support, and I/O failures that include the affected path.
