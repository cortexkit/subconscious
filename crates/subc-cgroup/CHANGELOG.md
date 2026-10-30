# Changelog

## 0.1.8

- Read memory charged to a module cgroup (`memory.current`), or, only when that file doesn't exist, sum resident and swapped memory across its processes and child cgroups. A malformed value or other read error fails the reading rather than falling back. Reads accept a deadline and never change controller settings.
- In a process sum, count a zombie's absent memory fields as zero while it still counts as a process, and skip and uncount a PID whose status read finds it gone. Cgroup memory is kept when swap accounting is absent. Present malformed counters and other read errors still fail the reading.

## 0.1.7

- Add a bounded module-subtree freeze with typed outcomes, a frozen guard that
  attempts a thaw on drop, and recursive process enumeration that excludes PID 0
  and init.
  Callers can signal listed processes before thawing without risking PID reuse.

## 0.1.6

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.5

- Give module cgroup directories an injective byte encoding and a dedicated `m-` namespace, preventing aliases and collisions with kernel interface files. Existing unprefixed directories are not reused; restart supervised processes when upgrading.

## 0.1.4 — 2026-10-02

- Added the safe `kill_module` API and typed `KillOutcome` for atomic cgroup v2 subtree termination, distinguishing a module with no cgroup placement, unavailable kernel support, and I/O failures that include the affected path.
