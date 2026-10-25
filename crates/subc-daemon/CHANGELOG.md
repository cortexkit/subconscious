# Changelog

## 0.37.8

- Bound ordinary supervised subc children to a first HELLO within 120 seconds of spawn, with per-module `register_timeout_s` from 10 to 3600 seconds. Expiry stops and reaps the child and records `registration_timeout` in terminal disposition detail as a crash, using the normal restart budget and slow retry. HELLO, child exit, operator stop/disable/removal and daemon shutdown cancel the wait. Non-wire children and the separate reload/swap deadlines are unchanged.
- Unix registration timeouts retain the unreaped process-group leader through the full TERM grace and group KILL, then reap it. Group ids come only from the recorded spawn pid, must exceed 1, and are rejected after reaping so pid reuse cannot target an unrelated group. Windows Job Object and Linux cgroup termination are unchanged.
- Send the client a per-route GOODBYE after `route.closed` when a newly added attested capability deny edge or target capability claim closes an existing route. This ends the client's route handle and settles accepted in-flight calls as outcome unknown instead of leaving them pending until their deadlines. Claim removal still leaves routes open.

## 0.37.7

- Keep health-probe and slow-retry uptime-reset deadlines absolute until their timer is polled, so a delayed executor poll does not postpone recovery. Paused-clock supervision tests now wait for the running process's current select and advance from its recorded uptime origin rather than treating `Running` as a timer-readiness barrier. The readiness timeout is unchanged.

## 0.37.6

- After a module exhausts its crash or health restart budget, retry it automatically after 10, 20, 40, then 60 minutes, capped at hourly retries. Each retry starts normally with a fresh restart budget. Healthy checks spanning the module's `RestartPolicy::window` reset the slow schedule. Operator start runs immediately; stop, disable, removal and daemon shutdown cancel pending retries. Rescans that retain the module retain its schedule. Timers use the daemon's monotonic clock, which pauses during sleep on macOS, so delays count awake time. `supervisor.list` includes an optional next-retry estimate for failed modules, and scheduling and starting each attempt produce INFO logs.
- Slow recovery now also covers daemon-initiated initial spawn failures, failed automatic respawns or previous-instance starts, wait errors, registration-release failures and stranded operations. Failures in operator start/restart/reload/swap operations and during shutdown do not arm retries. Operation ownership is carried explicitly through deferred respawns and failure helpers; repeated publication of one failure preserves its existing timer.
- Modules with no health checks reset slow-retry escalation after uninterrupted uptime spanning `RestartPolicy::window`. Modules with configured HTTP checks or advertised wire health checks still require healthy samples; a wire module waiting for HELLO is not assumed to have no checks. Both reset paths use the same monotonic awake-time clock.

## 0.37.5

- Shutdown always sends TERM before KILL: a late shutdown poll could kill a module without ever having sent TERM.
- Saving the running-module record no longer holds the live-roster lock across file and directory sync, so a slow disk can't make shutdown overrun its bounded wait for children. A process admitted while a record write is queued is published first, and a failed admission keeps its roster entry until the rejected child is killed and reaped.
- New INFO lines report shutdown drain and completion timings.

## 0.37.4

- Test-only: the role-declarations catalog test compares JSON values, not bytes. Workspace builds unify serde_json's `preserve_order`, so the byte comparison failed in CI; 0.37.3 was never published for that reason.

## 0.37.3

- Validate manifest `role_declarations` at HELLO for capability-name keys, object values and a 16,384-byte compact whole-map limit. Malformed or oversized maps are dropped whole with a WARN naming the module and reason, without refusing HELLO. Retain valid declarations in the registry and relay them unchanged through `catalog.list`, including after catalog updates, without interpreting any role's schema.

## 0.37.2

- Reset process-local health samples and consecutive failures on every accepted spawn, including restarts, starts after failure, and promoted swap candidates. HTTP-only children now get a fresh full probe cadence (plus the existing jitter) to start instead of inheriting an overdue probe from their predecessor. Wire children still wait for HELLO. Health-triggered restarts keep spending the same restart budget; sustained failures still end in `failed`. Escalation history and restart ledgers remain available across replacements.

## 0.37.1

- The `route.open refused` log line now names who asked, so a refusal can be attributed without matching connection ids, which restart with the daemon and belong to different modules before and after a restart. It gains `principal=` (the attested principal as on the `route.open accepted` line: `direct`, `reserved:<module>`, or `unattested` when a consumer identity's launch nonce did not verify; the claimed module id is never printed as a principal), `scope=` (`none`; the admitted stamp's label as on the accepted line once scope admission issued one; before that `requested/"<owner>"/"<ref>"@<epoch>`), and `harness=` (quoted). Every caller-chosen string is escaped and cut to 64 characters, so a refusal cannot forge a log line. Bodies, bind proofs and scope attributes are never logged. The launch nonce is now checked once at the start of a `route.open` instead of just before principal enforcement; which check refuses an open, and with which code, is unchanged.

## 0.37.0

- Minor bump because this crate's public API carries subc-protocol types, and subc-protocol moves to 0.33.0 (`ToolCallRequest` gains the optional `output_refs` member). Also takes subc-control 0.33 and subc-transport 0.14. Tool-call bodies, including `output_refs`, stay opaque to the daemon. No other change.

## 0.36.38

- Project manifest `operation_payload` declarations through `catalog.list` alongside `operation_delivery`. Empty declarations preserve the existing catalog response bytes; gateways can join declared limits onto negotiated federation catalog operations. Requires subc-control 0.32.7 and subc-protocol 0.32.12.

## 0.36.37

- A duplicate daemon start verifies an authenticating peer again after a 1.5-second settling interval before exiting successfully. If the peer disappears, the starter continues through normal singleton locking and binding instead of reporting success for a departing daemon. A remaining bind or ownership conflict still fails, allowing launchd's restart-on-failure policy to retry; a healthy peer still produces exit 0.

## 0.36.36

- Report the loaded configuration's `configured_enabled` in `supervisor.list` entries, omitting it for modules without a configured entry. Lifecycle acknowledgements are unchanged. This exposes temporary runtime start/stop overrides without changing reconciliation: every rescan or daemon restart reapplies the file's enabled values, even for unchanged entries. Requires subc-control 0.32.6.

## 0.36.35

- Test-only release; daemon behaviour is unchanged from 0.36.34, which was never
  published. Five new scope tests now build their expected folder the way the
  daemon stores it, in the plain Windows spelling rather than the verbatim
  `\\?\` form `std::fs::canonicalize` returns.

## 0.36.34

- Accept and canonicalize scope workdirs, validate opaque project ids and keep both fixed per epoch. Advertise `scope-workdir/v1` and stamp these fields only to declaring targets. Bound scoped workdirs join legacy live roots; activity copies scope folder/project without changing legacy fields.
- Workdirs are resolved before the scope table's write lock is taken, so a slow filesystem never holds up unrelated scoped route opens.

## 0.36.33

- Validate rich-UI descriptors during HELLO and recheck the registered descriptor before applying catalog updates; catalog updates preserve it. The daemon copies the descriptor from the registered manifest into `catalog.list`, and refusals identify malformed fields by name. Catalog entries expose `nonce_verified` only when HELLO passes the launch-nonce check required for a reserved module or blue/green swap candidate; registrations without a verified nonce keep their previous wire bytes.

## 0.36.32

- Successful rescans that add, remove or change modules append one `subc.config_changed` activity entry with sorted module ids and the caller's actor. No-op rescans, previews and refused checked applies remain silent; per-module rescan entries are unchanged.

## 0.36.30

- Serve `supervisor.rescan_checked`. A preview parses the candidate text with the loader used for the config file, compares it with the staged module specs under the supervisor operation lock, and reports each module's action (a launch-input edit on an enabled module is `restart`, a health edit or an edit to a module that stays disabled is `reload`, enable changes and enabled additions or removals are `start` or `stop`), the drain budget and declared busy gauges' cached readings for `restart` and `stop`, the modules still waiting for a restart from an earlier rescan, `restart_required`, `capability_warnings` (which also name a provider the candidate disables), and the `expect` to apply with. The apply reads the config file once under the lock and refuses `config_changed_since_preview` unless its SHA-256 and the staged generation match, then rescans those bytes exactly as `supervisor.rescan` does; only the apply asks for privileged-operation approval. Candidates over 1 MiB are refused `candidate_too_large`. The staged generation counter moves when an executed rescan's plan changes any module, before the first change is made, so it stays moved if that rescan fails partway.
- `server.describe` lists `rescan-candidate-preview/v1` and `config_path` only for a daemon started with a reloadable config file, and `config_path` only when that path is absolute. The op is announced by that capability and is not added to `subc_ops`, which `ck daemon --json` prints. Requires subc-control 0.32.3.

## 0.36.29

- Test-only release; daemon behaviour is unchanged. The test that holds a restart's live-children record write while a module authenticates spawned its module as `/bin/sh -c "exec sleep 600"`. On macOS every module goes through the privacy trampoline, which admits a module that is not a `#!` script only when its running image is the configured program; the shell replaces itself with `/bin/sleep`, which is not `/bin/sh`, so every process was refused and killed right after spawn and crash-looped with 100 ms, 1 s and 10 s backoff until the restart budget ran out. The test read the raw pid, which is published before that check, so it passed only when its 10 ms poll happened to see a replacement that was about to be refused; on a Mac it took 3 s, 14 s, or failed at its 20 s limit. The module is now `/bin/sleep 600`, and the test waits for `reported_pid`, which names a process only once it is confirmed as the module. With the module admitted the test passes in about 3.2 s, of which 3 s is the held write. The seven `health_event_tests` and `restart_queued_before_replacement_spawn_keeps_the_new_child` used the same shell-wrapped module and now run `/bin/sleep 600` too. The health tests wait for a confirmed pid and passed on macOS only because the image was read before the shell had exec'd `sleep`; with a 50 ms delay injected before that read, all seven timed out waiting for the module to run.

## 0.36.28

- `server::CONNECTION_EGRESS_BYTE_BUDGET` is now defined as `subc_protocol::MAX_DELIVERED_FRAME_LEN`. The router refuses any module-to-client frame larger than the client connection's egress budget, so the budget is the delivered-frame limit modules size their replies against; deriving it from the protocol constant keeps the two from drifting. The value is unchanged (4 MiB), so behaviour is unchanged. Requires subc-protocol 0.32.9.

## 0.36.27

- A planned daemon shutdown no longer logs a required capability as missing at ERROR (issue #139). During the ordered shutdown modules exit one by one, so a provider can deregister before a consumer that requires its capability; the requirement's verdict then becomes `never_provided`, and that was logged at ERROR although nothing was missing. Once the daemon has begun its announced shutdown (the same flag that stops respawns and records exits as `daemon_shutdown`), every `capability.requirement` event logs at INFO with `cause=daemon_shutdown`. Verdicts are computed exactly as before, so `route.open` refusals during the shutdown stay truthful. A provider that crashes or is stopped while the daemon keeps running still logs at ERROR.

## 0.36.26

- A restart no longer stalls the whole daemon while the live-children record is written. Admitting a spawned child and releasing a reaped one rewrite that record, syncing the file and its directory to disk, and a busy disk can make that take seconds. The write ran on a tokio worker thread, and the worker that runs a supervisor task woken by a child exit or a timer is the one that was polling the runtime's I/O and timer driver; while it was blocked no other worker took the driver over, so no socket or timer in the daemon moved. A replacement module that connected in that window got no server proof, and the Rust SDK's 2 s handshake deadline made it exit 1 (`Timeout { stage: ServerProof }`). Roster work that writes the record, or waits for the lock the write holds, now runs under `block_in_place` on a multi-threaded runtime, which hands the worker's other tasks to another thread for the duration. The write is still finished before a spawn is admitted, as before.

## 0.36.25

- The `route.open accepted` log line now says whether the open carried a scope: `scope=none`, or `scope=<kind>/<owner>/"<ref>"@<epoch>` from the stamp admission issued, with the ref quoted and cut to 64 characters. The bind proof and the scope's attributes are never logged.
- The operator confirmation audit line now carries the LocalAuthentication error domain and code (`os_error_domain`, `os_error_code`). A locked or non-interactive Mac (system cancel, not interactive) is reported as `no_presence`; a failed authentication or a user cancel is still a decline, any other error is `provider_error`, and nothing but an explicit success approves.
- Requires subc-presence 0.1.2.

## 0.36.24

- Test-only release; daemon behaviour is unchanged. 0.36.23 was never published: its Windows lint failed, because a log-capture helper used only by Unix tests was compiled on Windows too. It is now Unix-only. The caller-identity prompt test now holds its connecting socket in a dedicated child process: on Linux and macOS the lookup leaves a caller unresolved when more than one process holds the socket, and on Unix a test process that spawns children in parallel tests lends them copies of its own sockets until they exec. A new test pins an ordering that a client test had wrongly assumed: a configured module reads `starting` in `supervisor.list` until its first process is admitted, although that process may already be registered and listed in `catalog.list`. The startup-order test that waits for a `supervisor.set_enabled` reply now prints the in-process daemon's log when the reply does not arrive.

## 0.36.22

- When a restart, reload or swap drain runs to its ceiling, its timeout line now names why: for each declared busy gauge, its latest reading and when it was taken (`zero`, `non_zero(n)`, `not_a_count(value)`, `missing_from_health_body`, or `probe_failed(<kind>)`), the last value actually read, and how many probes ran (`gauge_probes`). A health answer that lacks a declared gauge is warned about once per drain, by gauge name; before, it only moved a counter. Drain behaviour is unchanged.

## 0.36.21

- A module frame bound for a client whose 4 MiB egress queue is full no longer closes the client at once. The daemon waits for room for at most 250 ms minus the age of the oldest queued frame, so a reading client that receives a large fan-out burst keeps its connection. A client whose oldest queued frame is already older than 250 ms has stopped reading and is closed at once, as before, so a stuck client still holds a module's frame reader for at most 250 ms. The same rule covers module GOODBYE delivery and the error that replaces an oversized reply, and each keeps its own close reason. The wait ends as soon as the client connection closes. Queue accounting stays lock-free: the age is that of the frame the writer is on, or of the frame that made the queue non-empty. The close warning now names the refused frame's type and flags and the close cause, never body bytes.

## 0.36.20

- Do not drop `supervisor.restart` requests received after a child starts serving but before its spawn generation is published. Coalesce requests only into a child created after the request, or a replacement still waiting to spawn, preserving protection against back-to-back restarts killing the same fresh replacement.

## 0.36.19

- Module `operator.confirm` refusals caused by the 30-second `(module, principal)` decline backoff include `detail.retry_after_ms`, rounded up to the remaining cooldown. Other refusal details are unchanged.

## 0.36.18

- Track acknowledged scoped binds per live epoch and expose owner-namespace `scope.served_by`; advertise `scope-served-by/v1`. Refused or unacknowledged binds never count and history is not persisted.

## 0.36.17

- Upgrade to subc-protocol 0.32.7 so HELLO accepts event names with one to four dot-separated tokens according to bus-naming 0.2.6. Modules must not declare dotted events until the running daemon carries this release; older daemons refuse dotted names at HELLO.

## 0.36.16

- Support attribution-only `ScopeAttributes.on_behalf_of_agent` from scope-authority owners. Reject conflicting agent/delegation/flow/run attributes, validate the agent token, and close scoped routes when attribution changes. Advertise `scope-on-behalf-of/v1` in HELLO_ACK and `server.describe`; send the attribute in bind stamps only to declaring targets and in `scope.describe` replies only to declaring callers. Legacy readers never receive the field.

## 0.36.15

- Project registered `ModuleManifest.operation_delivery` maps into typed `catalog.list` entries. Empty maps remain absent on the wire; unknown delivery strings are preserved. Watch hints naming missing or non-query operations do not refuse HELLO and are not subscribable according to the manifest helper. Supervisor replies do not expose manifests and are unchanged.

## 0.36.14

- With `test-support`, in-process harnesses can install the existing scripted operator-confirm provider through `ForwardingTable::configure_operator_script`, including optional event logging and timing limits, without bootstrapping a daemon process. The executable must still be named `ckdev-*`; other names retain the OS provider and log the same refusal as bootstrap. Builds without the feature are unchanged.

## 0.36.13

- Tests only, no behaviour change. Pin two scope-table rules that codemode run scopes rely on: a parent ending never ends a child, which stays live, drains with `scope_parent_ended`, and still admits binds stamped `parent_state: ended`; and re-pointing a child's `parent` at its parent's new epoch in the same sync keeps the link `linked` and drains nothing, while a re-point in a later sync makes it `linked` again.

## 0.36.12

- Write an INFO log when an intentional Unix SIGTERM shutdown finishes draining and ending children, so operators can distinguish a completed shutdown from a daemon that disappeared without a final record. Signal handling and the clean exit status are unchanged.

## 0.36.11

- At spawn, record a private fingerprint of the module's configured launch inputs (program, ordered arguments, the configured `env` block, protocol), excluding inherited environment and the per-spawn values the daemon injects. `supervisor.list` reports `pending_reload.spec`: `mismatch` after a rescan changes any of them until the module restarts, `unavailable` when no fingerprint was recorded for the running process. The fingerprint itself never leaves the daemon.

## 0.36.10

- A drain now re-checks a module's declared busy gauges every 250 ms while they read busy, instead of once per health cadence. The default cadence (30 s) equals the default drain ceiling, so a module busy at the drain's first check was never checked again: it waited the full 30 s and was cut with `drained: false` even after its gauges reached zero. Each check still starts only after the previous one finished and is still bounded by the drain ceiling; a missing or unreadable gauge still counts as busy. Health checks outside a drain keep their configured cadence.

## 0.36.9

- When a client connection ends because the peer closed it or a read failed, the daemon now gives its outgoing writer the same two-second flush grace as a requested close, then stops it. Before, teardown waited with no limit, so a peer that stopped reading could hold the connection's cleanup forever.

## 0.36.8

- At startup the daemon stops modules left running by a previous daemon that died. If such a module was identified and signalled, and its executable then becomes unreadable (usually because it is exiting), the daemon now keeps waiting for it to go. Previously it recorded the module as unidentifiable, which held back that module's restart until the old process disappeared. A module still running at the kill deadline is recorded as surviving, as before.
- On Windows, contain the daemon in a process-lifetime, non-breakaway kill-on-close root job before module creation. Suspended children inherit protection even before per-module job assignment, including `protocol: "none"` modules. Root setup failures disable launches; per-module containment failures kill and reap the suspended child rather than resume it unprotected. Service and scheduler host jobs must support nested assignment (Windows 8+).
- Durably flush live-children records before replacement and synchronize their containing directory on Unix (Windows has no directory sync, so a plain replacing rename is used there, which also tolerates a concurrent reader). Failed admission or confirmed-image publication now refuses and kills the child instead of continuing with an unrecorded process.
- A module's HELLO registration queues `subc.module_started` activity, but the ledger now waits for confirmation that its launch executed the module program. Late refusals discard queued starts; confirmed modules that exit immediately still count, and retained-build deduplication remains unchanged.

## 0.36.7

- Retain previous daemon children whose identity cannot be verified or that survive the stop deadlines. Their module ID alone waits for the previous PID to disappear, with a reason distinguishing unverifiable instances from instances that did not stop. Preserve the blocker and reason across roster rewrites; positive PID, start-time and image mismatches remain unsignalled and are not retained.
- Include module-egress queue admission in an explicit health probe's existing deadline and return `module_timeout` when admission stalls, so later requests on the caller's connection can proceed. Cancel activity-task-cap refusal sends when the connection closes. Other inline backpressure behavior is unchanged.

## 0.36.6

- On Linux, wait for the kernel's first-exec flag to clear before sampling a spawned child's executable for the live-children record. `posix_spawn` can return while `/proc/<pid>/exe` still names the inherited daemon image. Recording that image permanently prevented strict orphan cleanup after a hard daemon kill, including for `protocol: "none"` children that never register. PID, start-time and executable matching remain strict; PATH and shebang launches still record the actual kernel image.

## 0.36.5

- Always refuse module→client frames whose header plus body exceeds the client sink's total egress byte budget (4 MiB), even though previously they could pass through an empty queue. Oversized replies become daemon-origin `ERROR frame_too_large` on the same channel/epoch/correlation, ending only that request; other routes on the connection remain usable. Oversized reverse Requests are answered with the same ERROR to the module instead of reaching the client. This backstop complements the fleet's 3 MiB per-frame rule. A frame that would fit an empty queue but meets a full one is different: the client has stopped reading, so the slow client is still closed as before. The same applies when the replacement ERROR itself meets a full queue, so the module's frame reader never waits on a stuck client.
- An oversized Push remains the one oversize case that closes the client connection, with a WARN naming the module, client principal, channel, frame size and limit. Route-local recovery requires a dedicated `route.closed` reason and a decoder-first rollout because deployed Rust clients cannot decode new reason variants. Control and other egress admission accounting is unchanged.

## 0.36.4

- Slow `route.open` dispatch warnings now name the target module and attested opener and report time spent in admission and scope checks, project-root filesystem resolution, egress reservation, bind send, and the bind ACK wait. Fast opens emit no additional logs; routing and scope admission semantics are unchanged. Refusals before opener attestation report `principal=unresolved`.

## 0.36.3

- Require scope bind proofs only from non-reserved daemon-stamped openers, both at admission and before captured relay. Launch-attested reserved owners and carriers need no proof and ignore any proof sent. Their binds do not count toward idle-only rotation, and their `hold_presence` records no presence. Advertise `scope-bind-proof/v2` alongside v1 and `scope-replace-if-idle/v1`; owners can gate hashes on v2.

## 0.36.2

- With `replace_only_if_idle: true`, atomically refuse epoch replacement or same-epoch bind-proof change with per-record `scope_epoch_in_use` while the live epoch has routes, admitted binds in flight, or connection-held presence. An absent or false flag keeps unconditional replacement and its usual route drains. Scoped opens can hold presence until client disconnect or epoch end, surviving target restart; cap holders at eight, so further connections may bind but acquire no presence. Refused opens leave no presence. Advertise `scope-replace-if-idle/v1` in HELLO_ACK and `server.describe`.

## 0.36.1

- Advertise `scope-bind-proof/v1` in HELLO_ACK and `server.describe`. Owners should set `bind_proof_sha256` only when the connected daemon advertises this capability.

## 0.36.0

- **Breaking wire dependency upgrade.** Store optional scope bind-proof hashes. When a scope has one, every route open under it needs the matching proof: from the scope's owner and from every principal the owner lists as a carrier alike. It's checked at admission and again just before the bind is relayed to the module, in case the owner re-registered the scope meanwhile. Decode unpadded base64url of 32 bytes, hash the decoded bytes with SHA-256 and compare digest bytes in constant time. Without a hash, admission remains unchanged and proofs are ignored. Proofs never enter describe replies, logs, refusal details, activity attribution or module bind stamps; the hash stays out of describe and bind stamps.

## 0.35.0

- Shared peer sockets detected on macOS or Linux are described as "an unidentified local process" and use connection-keyed cooldowns rather than attributing requests to whichever holder is found first. Caller and executable-cooldown tests open their connecting socket in a re-exec child that never forks, so unrelated child spawns cannot make their identity ambiguous.
- Explicitly unlock the singleton start lock when its bootstrap owner is dropped. Closing the daemon's descriptor alone can leave an inherited open file description locked until a concurrent child execs; restart can now reacquire immediately. Blocking filesystem tasks still retain their lock guards until the protected work finishes.
- **Breaking.** Add the top-level daemon config key `privileged_operator_prompt` (`log` or `enforce`; omitted means `log`, any other value fails loading and rescan answers `invalid_daemon_config`). It adds the public `DaemonConfig::privileged_operator_prompt` field, the `PrivilegedOperatorPrompt` enum and the `RestartRequiredSection::PrivilegedOperatorPrompt` variant (`ALL` now has six entries), so struct literals and exhaustive matches must be updated. The key is restart-required: a rescan that changes it reports `privileged_operator_prompt` and the running mode stays until the daemon restarts, because the setting guards rescan itself.
- Classify every client channel-0 request in one exhaustive match. Restart, swap, reload, applying rescan, set_enabled and release_reserved are privileged; reads, `route.open`, `route.poll`, health probes and the rescan preview are not. A connection that owns a module registration still gets `unsupported_control_frame` for them, unchanged in both modes.
- In `log` (the default), every privileged client request runs as before and logs one INFO line naming the operation, target and caller that would have been prompted.
- In `enforce`, the daemon holds each privileged client request and asks the operator through the same provider, queue and limits as `operator.confirm`. It runs the request as received only on approval; a decline or withdrawal answers `operator_declined` and no possible prompt answers `operator_presence_unavailable`, both with the existing `detail.reason` strings. No wire code is added. Held requests do not block their connection's reader, and a disconnect withdraws the prompt and runs nothing. Linux keeps refusing with `operator_presence_unavailable`.
- The prompt text is built by the daemon from the operation, its target and the caller, never from client text, and fits the `operator.confirm` 200-character summary rule (`privileged_prompt::prompt_text`). An applying rescan's prompt promises no module count: the config is read after approval.
- Add `caller_identity::CallerIdentity`, which resolves the pid and executable of the process on the other end of an accepted loopback connection (libproc on macOS, `/proc` socket inodes on Linux, `GetExtendedTcpTable` on Windows, through `subc-os` 0.1.17). A failed lookup is described as "an unidentified local process" and never refuses a request on its own.
- Three person declines of privileged requests within a sliding 10 minutes from one executable (one connection when the executable is unresolved) start a 10-minute cooldown and log one WARN line naming the executable. During it, that executable's requests, including ones already queued and a reconnect, are answered at once with `operator_declined` and `detail.reason` `backoff`, without a prompt, and audited with `prompt_shown` false. Disconnects, withdrawals, timeouts and unavailability do not count, and refusals during the cooldown do not extend it.

## 0.34.10

- Change the reason to “approve a request from {module_id}: {summary} (requested by {opener}).” The macOS Touch ID sheet places that reason after “{app} is trying to,” so this verb phrase makes the whole sentence grammatical.

## 0.34.9

- Register every accepted configured module with the supervisor before publishing the connection file, in `starting` or `disabled` state. Start enabled processes and their monitors only after publication, so children can read their `--subc` file and discovery-time status requests cannot incorrectly report an accepted configured module as unsupervised. Preserve per-module health, restart and drain policies and record spawn failures on the existing status handle. Entries refused during registration are logged and skipped without preventing publication or startup of the other modules.
- Keep `configured module supervised` as the activation-time log, separate from the earlier registration log. Verify that a configured `protocol: "none"` child's first spawn persists its PID, start time and executable identity without a HELLO. The shutdown child roster (`ChildRoster::admit`) still writes `live-children.json` synchronously, with macOS executable confirmation following the trampoline's exec acknowledgement.

## 0.34.8

- Withdraws 0.34.7, which was never published: listing configured modules before the connection file is published returns in a later release with a fix for the orphan-sweep regression it caused. The code is identical to 0.34.6.

## 0.34.6

- Keep a crash's `route.closed` non-terminal when the supervisor has reserved the last available restart slot but has not yet published `Restarting`. Publish the pending-recovery fact atomically with the budget reservation, so connection cleanup cannot mistake the replacement's exhausted budget for the crashed process's recovery verdict. Terminal exit history still precedes the visible state change. Add a deterministic terminal-record barrier regression test.

## 0.34.5

- Publish the connection file at startup from tokio's blocking-thread pool instead of an async worker thread. The write fsyncs, and on a slow or busy disk that held a runtime worker for seconds (an in-process daemon in a test harness may share two workers with everything else). The order and guarantees are unchanged: the file is still written owner-only, fsynced and atomically renamed, only after the readiness probe authenticates, and the singleton start lock stays held until the write finishes, even if the startup future is dropped meanwhile. If the write task panics or is cancelled, startup fails with `ConnectionFileWrite` naming the file. Loading or minting the machine id, which fsyncs a newly minted file, also moved to the blocking pool, holding the start and run-directory locks the same way.

## 0.34.4

- Test only: the lifecycle provenance test waits for macOS exec confirmation before reading the running image, instead of failing when the status read comes first.

## 0.34.3

- List a module in `catalog.list`, and advance the catalog generation, only once its forwarding endpoint is installed and `route.open` can reach it. A HELLO now reserves the module's registry entry unpublished (the id is claimed, so a duplicate HELLO is still refused), installs the forwarding endpoint (queueing HELLO_ACK first, as before), then publishes the entry. Previously a client that opened a route as soon as the module was listed could be refused `target_unavailable` ("has no live forwarding connection"). While the entry is reserved but not yet published, `catalog.list` omits it but `route.open` and supervisor lookups still find it, so `route.open` answers `target_unavailable`, never `unknown_module`. A HELLO whose forwarding registration fails leaves the catalog generation unchanged. Swap candidates are published the same way, so the swap supervisor no longer sees a candidate as registered before the candidate's forwarding endpoint exists, which could fail the cutover. Add `Registry::register_unpublished_with_control_ops`, `register_unpublished_candidate_with_control_ops`, `publish_registration`, `published_registration` and `list_registered_modules`; the existing `register_with_control_ops` and `register_candidate_with_control_ops` still publish immediately.

## 0.34.2

- Record best-effort `subc.module_started` activity when a registered module's running-image identity differs from its latest start entry still present in the retained activity log. Compare the complete provenance evidence: Linux SHA-256 digest, macOS device/inode, or Windows volume/file ID. Include the tagged values in `details.old_evidence` and `details.new_evidence`, and identify the control requester as actor, or `reserved:subc` (the daemon itself) for a start with no requester. Count a skipped start only when no running-image evidence is available. A change between evidence methods, for example after moving a module to another platform, records a new start because identities measured by different methods cannot establish that the executable is unchanged.
- Record requested stop, restart/reload, swap cutover, disable and roster-changing rescan outcomes with the requester's verified principal. Refusals, no-ops, crash restarts and unrequested exits do not produce transition events.
- Queue lifecycle appends without waiting on storage or spending module rate credit. Count and warn with the event kind on failed or dropped appends; expose `activity_lifecycle_append_failures` and `activity_started_digest_skips` alongside the activity capability in `server.describe`.

## 0.34.1

- Add `ActivityConfig`, `load_with_activity` and a bootstrap retention builder without changing the public `DaemonConfig` struct or exhaustive wire enums. Module and activity policies are parsed from the same config document during bootstrap and rescan.
- Serve `activity.query` and `activity.subscribe` for Direct readers and nonce-verified `reserved:callosum` / `reserved:ckbus`. A registered module connection uses that module's reserved principal and ignores any supplied consumer identity, so only Callosum and ck-bus can read activity data through that connection. A separate consumer connection presenting no consumer identity is Direct, including one opened by a supervised module: the daemon cannot distinguish it from the operator's `ck` connection, so it retains local operator read access.
- Filter retained JSONL by agent, project, module, kind and inclusive time bounds, ordered by sequence with exclusive sequence pagination. Pin every query's current-incarnation head and file handles/lengths under the writer lock; scan after unlocking, off the connection reader. Count and skip malformed lines and repair incomplete tails on reopen.
- Replay successful appends from the current incarnation's 1024-entry ring and end a lagging feed after its queued entries with `activity_subscriber_lagged`. After restart, a query's head cursor combines the new daemon's incarnation ID with the previous daemon's highest retained sequence; it is accepted while no entry after that cursor has left the ring, but once eviction would leave a gap it refuses `activity_cursor_too_old` and requires query recovery. Foreign incarnation cursors refuse `activity_cursor_incarnation_mismatch`.
- Add `activity.max_age_days` (30) and `activity.max_total_mb` (64), both positive integers. Bootstrap uses these bounds and executed `supervisor.rescan` reopens the sink under the writer lock to apply changes without a restart; previews do not prune. The size budget includes the active file plus rotated generations, scaling per-file size for budgets beyond 256 MiB.

## 0.34.0

- **Breaking.** Upgrade direct dependencies to `subc-protocol` 0.31, `subc-control` 0.30 and `subc-transport` 0.12 together with the daemon: its public API passes their types to consumers. Replace `ScopeStamp` literals with its constructor and setters and account for optional `call_mode` in raw bind constructors and exhaustive patterns. Restart the daemon before scope owners send delegating runs. Providers must read every bind's mode, handle all three known modes and refuse unknown modes before declaring `call-modes/v1`; the daemon does not enforce per-tool mode restrictions.
- Require subc-protocol 0.31. Targets declaring `call-modes/v1` receive a scope-derived `call_mode` on every bind and `expires_at_ms` on stamps with deadlines. Legacy targets receive neither new field: non-delegating runs still bind through `agent-run-scopes/v1`, and flows through `flow-scopes/v1`. Both route admission and the captured relay endpoint require `call-modes/v1` or the matching legacy capability.
- Accept delegating run scope records, but refuse their binds unless the target declares `call-modes/v1`. Older run providers assumed no delegated agent authority; the refusal names that delegation and a relay refusal releases its reservation.
- Include the immutable epoch deadline in `scope.describe` stamps. Existing Rust stamp decoders accept extra fields, and Swift's management reply path preserves them. The TypeScript client has no describe API or reply decoder; its provider bind decoder ignores extra fields. Swift has no provider bind decoder.
- Update the record-validation test that formerly required `run_id` with `delegates` to be refused: delegation is now checked against each route's target, not rejected in scope records. Add daemon bind golden fixtures and reconnect tests that replace an admitted declarer with a legacy endpoint, verifying field omission and delegation refusal at relay re-check.

## 0.33.12

- On a daemon cut, `route.closed` keeps `reason: restart` and now reports the module connection's measured `drained` result, just like a single-module restart. A quiet route reports `true`; a drain that exceeds the two-second budget, fails, or has no recorded measurement reports `false`. Results belong to the connection drained by the cut, not a replacement registration with the same module id. `abandoned` still counts route.bind relays torn down separately and is not covered by `drained`.

## 0.33.11

- Correct activity tests that failed in CI: compare Windows project roots without the verbatim path prefix, and drop a test that read this changelog. No runtime change from 0.33.10, which was never published.

## 0.33.10

- Serve additive `activity.record` requests from registered modules and advertise `activity-ledger/v1` in HELLO_ACK and server.describe. The daemon verifies route ownership/epoch and live scope tags before recording the verified identity of the client that opened the route, its project and scope attributes. Events with no route or scope citation identify the calling module.
- Append `activity.jsonl` beside the terminal journal through cortexkit-log on a bounded single-writer queue. Replies follow successful writes, sequence numbers resume across retained rotated files, and reopening adds a newline after an incomplete final record so it cannot swallow the next append. Open/write failures and queue saturation refuse recording without holding connection readers or stopping routing, supervision or shutdown.
- Enforce module-owned kinds, Unicode scalar and compact JSON size limits, strict request shape and incarnation-long per-module token buckets (60 initial tokens, 10/s). Add controlled-clock, fault-injection and isolated TCP daemon tests.

## 0.33.9

- Sweep recorded Linux children whose PID and start time match when executable access is denied with EACCES/EPERM. Log at info that the executable is not readable (non-dumpable), without claiming image agreement. Missing recorded identities, PID reuse, different executables, and other read failures still refuse signalling; macOS and Windows are unchanged.
- Report permission-denied running images with a distinct unavailable reason after confirming the process start time. This remains unavailable, not an image match, for reload and upgrade decisions.
- On Linux procfs with `hidepid`, keep alive-but-unobservable orphans without signalling them. Merge their records across roster rewrites, block only their module IDs with an explicit previous-PID waiting reason, and keep unrelated modules serving. Recheck existence with signal-zero about every five seconds; only ESRCH releases the record and permits normal configured startup, without spending crash-restart budget. Start, restart, reload, swap, and route requests report the waiting reason while that previous instance exists.

## 0.33.8

- Record terminal history before publishing a health-budget failure or clearing the reaped child's process facts. Apply the same ordering to every child-bearing drain, ordinary exit and daemon-shutdown exit; failed waits and drain teardown errors record their unknown exit before publishing failure. Journal errors still retain the exit record in memory and publish the requested stopped, disabled, restarting or failed state. A panic during terminal recording also publishes that state before resuming the unwind. No-child spawn failures remain failures without fabricated exit records.
- Add a test-only socket barrier at the terminal writer to prove health-budget and ordinary exits cannot publish failure or process death before history is available, plus health-budget journal-error and recording-panic coverage.

## 0.33.7

- Give each Windows module process a separate named pipe, including a replacement started alongside a still-serving module. Before writing the nonce, the server requires the connected process's PID to equal the PID returned when the daemon spawned that module. Keep `SUBC_LAUNCH_NONCE` and `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` until every module reads the pipe, so failed pipe preparation or delivery does not prevent registration. Only after the client reads every byte does the daemon record `pipe` in `supervisor.list` and `supervisor.provenance` under `daemon_observed.launch_nonce_source`; module list/status displays that record.
- Keep the process handle used for Windows executable-identity and resource checks alongside the pipe handles. Remove the fallback permission with the environment copy once every module reads the pipe; keeping the copy still exposes the secret to other processes running as the same user.

## 0.33.6

- Compare Windows configured and spawned paths after OS canonicalization so ordinary and verbatim spellings do not falsely require a reload. A different resolved configured program still requires a reload even when the child's executable matches the file at its original spawn path.
- Preserve Windows launches by explicit path to extensionless executable images; only try the `.exe` suffix when the exact path is absent.
- Capture Windows executable identity before suspended spawn and retain the child's handle for provenance and resource readings. Provenance now reports match, mismatch or unavailable using creation time and the full file ID; orphan cleanup only forces a stop on a matching creation time, and waits for exit. Persist Windows file IDs as additive version-1 fields so rollback readers keep reading the roster; Unix records and supervision are unchanged.

## 0.33.5

- Advertise `catalog-events/v1` in `server.describe`, so a consumer can tell a catalog entry without `events` (the module declares none) from a daemon that does not report declared events.
- Create Windows run directories with protected owner-only DACLs instead of trusting inherited permissions, including redirected paths. Tighten existing run directories and remove broad inherited access from unprotected descendants using `subc-os`.

## 0.33.4

- Include each registered module's declared bus events in `catalog.list` without changing entries whose manifests omit events.
- Rate-limit pre-authentication rejection logs per listener instead of process-wide, so one listener's rejects can no longer suppress another listener's first line.

## 0.33.3

- Keep an exited child's stdout reader running to EOF when it exceeds the 250 ms restart wait, just like stderr. Aborting that reader discarded output still buffered in the pipe, which could leave Windows capture files permanently short after a burst. The restart remains bounded; late output from either pipe may arrive after the replacement starts.

## 0.33.2

- Log pre-authentication rejections at info level with the peer, elapsed time, and a reason that distinguishes capacity waits, handshake deadlines, invalid proofs, malformed messages, and peers that close early. Rate-limit each reason and include its accumulated suppressed count on the next permitted line.

## 0.33.1

- Fix a route that `route.open` reported as open but that silently dropped the client's first requests. When a module answered the route bind very quickly, the daemon could commit the route and then, a moment later, also tell the module to drop it (a GOODBYE on the route's channel). The client got "accepted", the module logged `dropped module frame without a matching route binding cause=unbound_channel` for the client's requests, and the client waited forever for answers. The daemon now sends that GOODBYE only for a bind it really abandoned (the client went away, or the module began draining, before the module answered), and never for a route it committed. A bind the module acked for a client that had already gone still gets exactly one GOODBYE.

## 0.33.0

- Enforce scope deadlines (`expires_at_ms`) on wall-clock time: an expired scope ends with every route under it closed as `scope_ended`, and is refused if resent. A route.open under a scope past its deadline is refused `scope_not_live` even before the next sweep. The expiry check runs only while some live scope has a deadline, so a daemon without deadlines never wakes for it.
- Serve `scope.apply` (advertised in `subc_ops`) and the `run_id` rules, with the `agent-run-scopes/v1` admission gate checked at admission and again on the captured relay connection.
- **Breaking.** Requires `subc-protocol` 0.30, `subc-control` 0.29 and `subc-transport` 0.11, whose types appear in this crate's public API.

## 0.32.16

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.32.15

- Log each mutating supervisor request before handling it, including the daemon-verified caller principal and connection id. Preview rescans and read-only requests are not logged.

## 0.32.14

- Log each module registration ending (`module registration ended`, with the reason: connection closed, goodbye, supervisor stop or restart, failed registration, or replaced), and log when a swap restart promotes the new process's registration over the old one (`module registration promoted`). The existing `module registered` line is unchanged.

## 0.32.13

- Park running children that have no health check instead of polling registration every 10 ms. For modules that register over the subc wire, health probing now starts and stops on registry notifications (HELLO, reconnect, disconnect, catalog updates and swap promotion) instead of a timer; live HTTP health configuration still changes through supervisor commands. Non-wire children without HTTP health wait only for exit or commands. Unknown-health snapshots are written only when their fields change. The advertised-health jittered cadence (30–33 seconds by default) and exit/registration-release waits are unchanged.

## 0.32.12

- Test only: three supervisor tests no longer fail when tests run in parallel. The test log capture keeps a second log dispatcher alive, so a parallel test with no subscriber can't make the logging library cache a log line as disabled; the exec-status test waits for the module's process id instead of reading it once; and the warming-snapshot test uses a fixture with no live supervisor task.

## 0.32.11

- 0.32.10 was never published (its CI failed on a Linux lint error); this release carries its changes plus that fix.
- Connect `operator.confirm` to the isolated macOS LocalAuthentication and Windows Hello/CredUI providers. Only a successful OS authentication confirms; errors remain fail-closed. Linux still returns `unsupported_platform` without a prompt. Withdrawing a prompt that is on screen is supported only after the person-present checks in subc-presence's README pass on that platform.

## 0.32.9

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.32.8

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.32.7

- Serve and advertise the additive `operator.confirm` operation with nonce/route permission checks, validated summaries, a bounded FIFO queue, decline back-off, independent withdraw/timeout/stuck timers, and bounded answer delivery. Audit every outcome with an escaped summary. Until a platform prompt is installed, every request is answered `operator_presence_unavailable`, so nothing can be approved by default. The scripted provider used by tests exists only with the `test-support` feature and only for daemons running under a `ckdev-` name, so a production `ck-subc` can never approve through it.

## 0.32.6

- Validate optional module event declarations at HELLO before registry admission, refusing malformed blocks with `invalid_event_declaration` and a field-naming reason. Requires subc-protocol 0.29.3, whose manifest decoder retains `events` in the registry alongside the other manifest data. Older modules without events are unchanged.
- Declarations are **not yet served** in `catalog.list`, `server.describe`, or `ck catalog`; serving lands in the next subc-control minor release. Older daemons drop the field. Deploy readers before publishers depend on event discovery; this validation-only release is not a discovery guarantee.

## 0.32.5

- Record the exhausted crash budget's terminal history before publishing the module as `failed`, so status readers cannot observe the failure before its exit record. Journal write failures still leave the module failed; an exit-recording panic publishes failure before continuing its unwind. Commands and health actions remain serialized by the supervisor, and journal I/O does not hold the status snapshot lock.

## 0.32.4

- On macOS, a `#!/bin/sh` module is recognised as its own orphan after a daemon crash even when the system shell launcher has re-executed the selected bash, dash or zsh interpreter. The selected interpreter's file identity is saved at launch, not resolved again after a crash or upgrade. Other image differences, reused pids and changed start times still refuse cleanup. Previous daemon versions can read the extended roster but retain their stricter single-image matching.

## 0.32.3

- Add deterministic macOS privacy boundary tests: one holds a module launch while the privacy trampoline (the daemon's own binary, before it execs the module) is still running, and checks that the live-children roster doesn't record the trampoline as the module; the other makes a module exit 121 on its own right after a successful exec and checks it's recorded as the module's exit, not a trampoline refusal. The sampling seam exists only in unit-test builds and the exec barrier only in the fixture binary; production behaviour is unchanged.

## 0.32.2

- `ck module list` and cached health now show `unknown` as soon as a wire module fails a health probe, instead of continuing to show its last `ok` report. A later successful report restores the reported health. Restarts still wait for the configured consecutive-failure threshold; route admission and HTTP-probed `protocol: "none"` modules are unchanged.

## 0.32.1

- On macOS a module's pid first runs the `ck-subc` launch trampoline, which replaces itself with the module. Status, provenance and resource readings now leave that pid out until the exec acknowledgement confirms the module image, so they never report `ck-subc` as the module. The spawn/exit feed still reports physical process facts, while status and provenance report a confirmed module.

## 0.32.0

- macOS behaviour change: every supervised module (wire or `protocol: "none"`, including swaps) launches as its own responsible process. Privacy prompts name the module; grants to `ck-subc` no longer reach modules. There is no opt-in or opt-out. Grants follow the module's code signature, so modules needing Screen Recording, Accessibility, Files and Folders, Local Network or Full Disk Access must use a stable team signing identity, not ad-hoc signing which prompts again after rebuilds. Linux and Windows are unchanged.
- Require embedded macOS supervisors to configure an explicitly probed privacy trampoline. Missing symbols and trampoline failures refuse launches by name while the daemon control server remains available. Preserve pid, stdio, process group and fd-3 launch nonce with SETEXEC, and asynchronously confirm the executable before recording it for orphan cleanup. Per-spawn INFO/WARN logs expose the identity and refusal cause.
- Existing releases ignore unknown per-module config keys. This release adds no privacy config key and requires no configuration rollout ordering.
- The launch acknowledgement pipe tells a trampoline refusal (a tagged record naming the cause) from a successful launch (the pipe closes with no bytes), so a real module that exits immediately with codes 120–123 keeps its normal exit record.
- Isolate the run-directory lock release test from concurrent forks. A forked child can temporarily retain the flock even when the descriptor is close-on-exec; a deterministic process test observes that interval and verifies release at the trampoline's first exec. No production lock or spawn behaviour changes.

## 0.31.1

- Refuse a flow-scoped `route.open` before relaying any bind unless the target's registered manifest provides `flow-scopes/v1` in `capabilities.provides`. The refusal is terminal `target_flow_unsupported` and names the target and the capability. The check runs at admission and again against the connection the relay actually captured, so a replacement module cannot inherit its predecessor's declaration. The daemon never strips `flow_id`. Scopes without `flow_id` and unscoped routes are unaffected.

## 0.31.0

- Takes subc-protocol 0.29.0 and the matching control/transport minor releases: `ToolCallRequest.preset` and `ScopeAttributes.flow_id` are optional wire fields, but this release is breaking for Rust struct literals. Tool-call bodies, including presets, remain opaque to the daemon.
- Only owners listed in `scope_authority_owners` may set `flow_id` in `scope.sync`; any other owner's record is refused with the same `scope_attribute_not_permitted` as an unauthorized `agent_id`. A malformed flow id refuses the whole sync with `invalid_control_body`, and the message names `flow_id`. A valid one is copied verbatim into the scope stamp the daemon attaches when a module accepts a route (`route.bind`), even when the scope has no agent. Changing a flow id within the same scope epoch raises the scope's content version and closes every route under the scope with `scope_delegation_changed`.

## 0.30.0

- Breaking Rust API: `HealthConfig` owns an optional HTTP URL and is `Clone`, no longer `Copy`; `ForwardingError` gains `ConnectionRoleConflict`, so exhaustive matches must handle it. Requires subc-jobobject 0.2.0 on Windows and subc-cgroup 0.1.5 on Linux.
- Reject reserved capability claims in `catalog.update` before changing active or swap-candidate registrations, using the same `reserved_capability` error as HELLO.
- Module GOODBYE now performs disconnect teardown immediately, including requirement recomputation, route-closed client pushes and scope-sync authority release, without waiting for the socket to close.
- Scope sync propagates parent refusals through direct dependencies and shares ancestor walks (including shared cyclic ancestry), avoiding cubic work on leaf-first scope chains.
- Route-bind breaker probes now own a unique admission token; settling an older relay cannot release a newer probe or misreport it as a failed probe.
- Requirement episode numbers remain monotonic for each consumer/capability pair when a consumer is disabled or a declaration disappears and later returns.
- Runtime supervisor start/stop overrides configuration defaults when evaluating enabled capability candidates, cached consumer requirements and satisfiability; starting a config-disabled provider now gets the normal pending window.
- Reserved-capability refusal warnings describe the current attempt only; unrelated catalog updates no longer re-log historical claimants that never registered.
- Fleet lint still reports duplicate module IDs as operational failures but no longer labels them as modules that do not expose a manifest.
- Rescans preserve a running process's launch protocol until its next spawn, including liveness, health probing, and clean-exit classification.
- Managed `protocol: "none"` modules can opt into a loopback-only `health.http` probe. Health endpoint, cadence, deadline, and failure threshold changes apply live on rescan; failed HTTP probes use the existing consecutive-failure restart policy. Config refuses HTTPS, non-loopback hosts, and HTTP probes on wire modules. `HealthConfig` now owns an optional URL and is `Clone`, no longer `Copy`.
- Ephemeral IPv6 bind collisions fall back to the bound IPv4 listener; fixed-port collisions still fail. On macOS the open-file limit is clamped to the kernel's per-process ceiling.
- Daemon discovery is published after orphan cleanup and authentication serving are ready. The low-level singleton binding API retains its existing publication contract.
- Orphan records identify the running executable image after PATH lookup and shebang interpretation, while provenance still records the configured program.
- Swap retirement drains and journals the incumbent in the background, leaving the promoted process supervised and operator commands responsive. Linux kill domains are unique per spawn, and reaping cleans the process tree and its cgroup.
- Stderr read failures stop marking the tail incomplete after their historical process section is evicted; failures in retained history remain visible.
- Refuse a second HELLO on a registered module connection without changing its identity, launch nonce, or existing routes.
- Serialize module registration and client-route reservation admission, refusing mixed roles with terminal `invalid_hello` or `invalid_request` rather than allowing unusable, leaked routes.
- Settle late accepted route binds during daemon drain with `module_reloading`, releasing both reserved channels and preserving the shared module connection for ordered shutdown.
- Repair `bench-harness` sink receivers to consume accounting-bearing `OutboundFrame` values, restoring feature compilation.
- Preserve FIFO order when the test-only dispatch prototype (`dispatch_spike`, built only for tests) reuses a cancelled correlation id, by fencing queued entries with request-incarnation tickets.

## 0.29.2 — 2026-10-02

- Close registrations whose sockets outlive a reaped supervised process before restarting. Failed release attempts leave an operator-revivable state; start also revives Restarting modules with no child or scheduled respawn.
- Serve supervisor commands during health, operator, and reload retry backoffs, allowing disable or drain to cancel the replacement. Reload acknowledgements still wait for registration, or report cancellation.
- Health restart budget exhaustion leaves the module enabled and failed, with the limit and window in its terminal record. Keep a reaped reload child's roster entry until its terminal record is written so daemon shutdown cannot exit first.

## 0.29.1 — 2026-10-02

- Linux module teardown atomically kills the contained process tree with `cgroup.kill` after waiting for graceful module shutdown. If cgroup delegation or kernel support is unavailable, teardown kills only the direct child as before. I/O failures produce a warning but do not change the teardown result.

## 0.29.0 — 2026-10-02

- Lifecycle control pushes name exactly the client-side channels covered on each receiving connection, including scope revocations, module drains and daemon shutdown. Route GOODBYE still carries no body; the reason is conveyed only by the control push.
- Minor bump because this crate's public API carries `subc-control` types and moves to `subc-control` 0.27 (`RouteClosing` and `RouteClosed` gain `channels`). A consumer that also depends on `subc-control` directly must move both together, or two incompatible copies would meet.

## 0.28.1 — 2026-10-02

- The warning for a retired `launch_nonce_env` key now goes to the daemon's log (`run/logs/subc.<date>.log`) with the module named, not to stderr. Under launchd and systemd the daemon's stderr is usually discarded, so in 0.28.0 the warning reached no operator.

## 0.28.0 — 2026-10-01

- Unix supervised wire modules now receive launch nonces only through the inherited pipe and `SUBC_LAUNCH_NONCE_FD`; the daemon never supplies `SUBC_LAUNCH_NONCE`, including on swaps. Windows retains its environment handoff because std cannot restrict inherited pipe handles to one child.
- Removed `launch_nonce_env` from daemon config and the public module launch spec. Existing config entries still load and log a module-named deprecation warning for one release. The status wire field remains for that release as a platform constant (`false` on Unix, `true` on Windows); it no longer controls a spawn or pending reload.

## 0.27.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet. Also takes `subc-control` 0.26 and `subc-transport` 0.9, which moved for the same reason.
- `route.open` checks `role_versions` with `subc_protocol::session::validate_role_versions` before anything else. A malformed map is refused as terminal `invalid_request` with `detail.field = "role_versions"`, and the module never sees a bind. An empty map becomes no field. A well-formed map is forwarded unchanged on the module's `route.bind`.
- `route-role-versions/v1` is advertised in HELLO_ACK and `server.describe`.
- The route.open refusal counter gains the `invalid_request` key.
