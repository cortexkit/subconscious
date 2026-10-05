# macOS module privacy identity

On macOS every supervised module, including `protocol: "none"` programs and
blue/green swap candidates, becomes its own responsible process. This is a
behaviour change: Screen Recording, Accessibility, Files and Folders, Local
Network and Full Disk Access prompts name the module, and grants previously
given to `ck-subc` no longer reach modules or their agent shells. There is no
config opt-in, exception or opt-out. Linux and Windows launch behaviour is unchanged.

The supervisor still uses its single Tokio Command path, including environment,
stdio, process group, kill-on-drop and the final fd-3 launch-nonce handoff. On
macOS its program is the explicit privacy trampoline with hidden first argument
`__disclaim-exec`. Before any runtime, logging or configuration, the trampoline
uses Darwin `posix_spawnp` with `POSIX_SPAWN_SETEXEC` and the dynamically resolved
private `responsibility_spawnattrs_setdisclaim(&attr, 1)`. SETEXEC replaces the
image in place: the pid, group, pipes and non-CLOEXEC nonce descriptor survive.
No `POSIX_SPAWN_CLOEXEC_DEFAULT` flag is added. The nonce handoff explicitly
clears close-on-exec, including when its original descriptor is already fd 3.

## Fail closed and executable identity

At startup the supervisor probes its explicit trampoline once. It must resolve
the responsibility symbol and return the fixed capability response. A missing
symbol or wrong binary produces one ERROR; the daemon keeps serving `ck`, but
each module launch refuses with the named cause. Each trampoline also resolves
the symbol independently. Lookup, attribute setup and spawn failures print one
stderr line and exit with reserved codes 120–123. **Never fall back to ordinary
exec:** that would silently give a module the daemon's grants again.

A separate acknowledgement pipe has its write descriptor above fd 3 and stdio.
It survives the first exec; the trampoline makes it CLOEXEC for SETEXEC. The
parent drops its writer immediately after spawn. The asynchronous child monitor
waits at most five seconds for EOF, checks whether the child exited, then
compares the kernel image's device/inode with the resolved module executable.
The supervisor refuses a missing or mismatched image, kills the fresh process
group, and records the named spawn failure under the usual restart budget. A
script retains the existing orphan-identity rule: the kernel's selected
interpreter image is recorded, but the trampoline image is never accepted.
A script module's privacy identity is its interpreter's, so a script needing
permission should be shipped as a signed binary.

Before confirmation the shutdown roster owns the pid but records no executable.
Orphan cleanup cannot signal such an unverified record. After confirmation it
records the module, never the transient `ck-subc` image. This preserves the
existing synchronous Supervisor API without blocking a Tokio worker on the
acknowledgement. Normal shutdown can still end an unconfirmed child it owns.

Embedded macOS daemons must explicitly call
`Supervisor::with_privacy_trampoline` (or `BootstrapConfig::with_privacy_trampoline`)
with `ck-subc` or a binary calling `subc_os::privacy_identity::trampoline_main`
before any runtime. The library never assumes an arbitrary host executable
implements this protocol. Missing configuration refuses by name.

## Signing and operator visibility

Privacy grants follow the module's code signature. Modules requiring permissions
must be signed with a stable team identity: ad-hoc signing has no stable developer
identity, so every rebuild prompts again. A long-lived, team-signed disclaimed
process can obtain Local Network permission and retain it across reruns (measured
2026-10-05). macOS names a process for a privacy prompt only while it is still
running, so a short-lived probe is never prompted and is not evidence that a
module can't be granted.

Each confirmed spawn logs INFO with module_id and pid:
`module spawned with own privacy identity (responsibility disclaimed)`.
Refusals log WARN with their cause; the trampoline's stderr also enters the
module capture log. Supervisor diagnostics never pollute the child's own output.
`ck module logs <id>` exposes these facts. Identity will enter `supervisor.list`
when `subc-control` next takes a breaking release; at that point `SupervisorEntry`
should become `#[non_exhaustive]` with a constructor so later additions do not
force another fleet-wide source break. Status does not claim that pending disk
configuration is the identity of a currently running process.

Existing daemon releases ignore unknown per-module config keys rather than
rejecting them. This cutover adds no key, so no config rollout ordering is needed.
