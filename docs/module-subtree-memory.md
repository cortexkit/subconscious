# Module memory including child processes

`supervisor.list` entries may carry `subtree` beside `resources`. The existing
`resources` reading still describes only the process the supervisor started;
none of its fields change. Older readers ignore `subtree`, and an absent
`subtree` means the daemon does not report it. An absent member is not serialized.

Every subtree reading has a `source`:

| Source | Fields | Meaning |
| --- | --- | --- |
| `cgroup_memory` | `memory_bytes`, optional `swap_bytes`, `process_count` | Used whenever the module cgroup has a `memory.current` file. Linux cgroup v2 `memory.current` and, when swap is accounted, `memory.swap.current`. `memory.current` includes page cache charged to the cgroup. A missing swap interface leaves swap absent without discarding memory. The count includes processes in child cgroups. |
| `process_sum` | `memory_bytes`, optional `swap_bytes`, `process_count` | Linux fallback, used only when `memory.current` doesn't exist; a malformed value or other read error fails the reading instead. Sum `VmRSS` and `VmSwap` from `/proc/<pid>/status` for the PIDs in `cgroup.procs` throughout the module cgroup subtree. Shared pages count once per process, so this is not unique physical memory. On Windows it is the sum of working sets of the module job's processes; `swap_bytes` is absent because Windows has no matching per-process swap figure. |
| `unavailable` | `reason`, optional `process_count` | No safe container-wide reading could be obtained. A count already obtained is retained when a later read fails. No single-process reading is substituted. |

`process_count` is always present when processes were counted. An absent
`swap_bytes` means not accounted, not zero. Process membership and memory can change
during a read, so the figures are observations, not an atomic snapshot.
Linux's `VmRSS` is a process's resident-set size (pages present in RAM); `VmSwap`
is its swapped-out private anonymous memory and does not include shared-memory swap.
In a process sum, processes whose status lacks either memory field, including zombies, contribute
zero for that field and still count. A PID that exits between enumeration and its
status read (`NotFound` or `ESRCH`, the no-such-process error) is skipped and excluded from the count. Present
but malformed fields and all other read errors remain failures.

On macOS the source is `unavailable`: children that leave the module's process
group cannot reliably be found. Linux modules without cgroup placement are also
unavailable. Read failures are reported rather than converted into zero usage.
Read work runs away from the supervisor and connection reader. Every subtree
target gets its own 200 ms budget, and targets are read independently. A target
that does not finish reports `reason: "timeout"`; other targets' completed readings
are retained. There is no shared deadline that expires later modules' budgets.
Only two subtree targets per list enter its read pipeline at once, so later targets'
budgets do not elapse while earlier targets are being read.
At most two blocking readers run at once, including any timed-out readers still
waiting in a filesystem call; such a reader keeps its slot until its call returns, so it can delay other targets. The budget bounds how long a reply waits for a target, not how long the read runs, and the whole list can take longer than 200 ms. Each target waits only until its deadline for a slot,
then reports `timeout` rather than queuing more blocking work. Other reasons are `not_running`,
`no_cgroup`, `no_job`, `unsupported_platform`, `unsupported_source` and
`read_failed`.

Each existing per-process `resources` read gets a fresh 200 ms budget when that
module's read starts, sharing only the global worker cap. Time spent on earlier
modules, status snapshots, or image probes does not consume that budget.
A slow process read makes that field `unavailable` with its existing
`unreadable` reason, rather than waiting indefinitely. Successful process reading
bytes are unchanged. Timeout results apply only to that reply: nothing is cached,
and the next list reads afresh. Timeouts are logged at debug level with the module
ID and read kind, at most once a minute per module.

These figures are for operators and monitors only. They do not impose limits,
trigger restarts, or change supervision policy. `ck module status` labels them
as memory including child processes and includes the source. `ckni`'s versioned
JSON preserves the `subtree` member from the daemon.
