# subc-presence

An OS-binding leaf crate used by the daemon on macOS, Windows and Linux (the CLI
also shares its Linux policy renderer). It denies unsafe code by default and
enables it only on individual
FFI items. The daemon owns authorization, deadlines, cancellation reasons and
audit; this crate only supplies a blocking prompt and a cross-thread withdraw
handle. An explicit OS authorization is the only approving outcome.

- macOS creates a fresh `LAContext` for every request and evaluates
  `DeviceOwnerAuthentication` (Touch ID or password). Withdrawal sends an
  invalidation command to the context's blocking thread and waits for the reply.
- Windows probes Hello availability and the input desktop **before creating any
  window**. The four-second receive limit also covers stuck COM activation; one
  bounded worker prevents accumulating stalled probe threads. Only Hello
  `Verified` approves. Without Hello, secure-desktop CredUI credentials are checked
  with `LogonUser`, and the resulting SID must match the daemon's current process
  user. Credential buffers are wiped before being discarded. Withdrawal cancels
  the Hello operation and dispatches owner-window destruction to its creating
  thread. The prompt call waits for the actual Hello/CredUI return, not merely a
  cancellation request. If that never returns, the daemon's independent stuck
  timer keeps later requests unavailable and never reuses the prompt slot.
- Linux uses a blocking, pure-Rust system D-Bus client and a fresh polkit check
  for every request. See [Linux setup and verification](#linux-setup-and-verification).
- Other platforms return `UnsupportedPlatform` without publishing a handle.

## Person-present release checks

The checks below need a person at the machine and can't be inferred from unit
tests: approving, cancelling, the password fallback, and withdrawing a prompt
that is on screen. In particular, **Windows
secure-desktop dialog dismissal on owner destruction is unmeasured** until the
interactive check passes; a dialog that remains after withdraw is a failed check.

On macOS, run:

```sh
cargo run -p subc-presence --example ckdev-presence -- 'approve this smoke check'
cargo run -p subc-presence --example ckdev-presence -- 'withdraw this smoke check' 5
```

Use the first command three times: approve, Cancel, then Use Password and approve.
Use the second command without responding and verify the window disappears.
Record the machine, OS version/build, date, rendered sentence and printed outcome.
The exact reason sentence is printed before prompting. The example exercises the
provider directly and does **not** emit a daemon audit line; the integrated daemon
check must separately verify an `operator_declined` audit outcome on withdrawal.

On Windows, use the same commands for Hello approve/cancel/withdraw; repeat on a
machine/account without available Hello for CredUI approve/cancel/withdraw.
Record the Windows build and which dialog, if any, failed to disappear. Unit tests
on the existing Windows CI leg include the real, no-window pre-check with a 5 s
deadline; they do not show either authentication dialog.

## Linux setup and verification

Run `ck setup operator-prompt` as the operator. On a terminal it uses interactive
sudo to install only
`/usr/share/polkit-1/actions/io.cortexkit.subc.elevate.u<uid>.policy`, owned by root
with mode 0644. Without a terminal it prints a complete shell recipe, including
the XML, for administrator approval; it changes nothing. Remove the same file
with `ck setup operator-prompt --uninstall`. Missing setup refuses with a distinct
reason and the CLI suggests the setup command. No polkit service restart is needed.

The per-user action names that uid as its owner, allowing the daemon to supply
details. Its defaults are `allow_any=no`, `allow_inactive=no` and
`allow_active=auth_self`, **never** `auth_self_keep`: a retained grant would not
bind the next request's text. An administrator's separate polkit rules can change
these decisions; setup does not install or alter any rules.

The subject is the daemon's process, preferring a D-Bus Unix-FD pidfd and carrying
pid/start-time/uid for older polkit versions. polkit's systemd session monitor
can map a session-less user service to its uid's Display session. This avoids
assuming that the user service itself has a logind session. The daemon never
accepts a result from an authentication agent's own authorization call.

Everything the person needs to see is in `polkit.message`: a fixed
`$(cortexkit.request)` template whose detail value is the complete daemon sentence.
Upstream polkit 126 performs substitution once, appending values without
re-expanding them, so module text containing `$(x)` stays literal. See
[the expansion loop](https://github.com/polkit-org/polkit/blob/126/src/polkitbackend/polkitbackendinteractiveauthority.c#L1990-L2064),
[caller trust](https://github.com/polkit-org/polkit/blob/126/src/polkitbackend/polkitbackendinteractiveauthority.c#L945-L986),
[agent selection](https://github.com/polkit-org/polkit/blob/126/src/polkitbackend/polkitbackendinteractiveauthority.c#L1722-L1787)
and [session lookup](https://github.com/polkit-org/polkit/blob/126/src/polkitbackend/polkitbackendsessionmonitor-systemd.c#L380-L428).

A non-interactive preflight distinguishes a policy denial (no presence) from a
person declining. A challenge proceeds to an interactive check. An authorized
non-interactive probe is an administrator's policy-rule grant, not a person's
approval: the generated `auth_self` policy has no retained grants. It returns
`Outcome::AuthorizedWithoutPrompt`, distinct from the interactive check's
`Outcome::Approved`, so callers can require an interactive confirmation. Linux refuses
an effective-root daemon with a `root_user` provider error before any D-Bus call,
because polkit otherwise authorizes root subjects unconditionally.
Authorization is
the only approving result; no agent, errors, withdrawal and daemon timeouts never
approve. Withdrawal uses the same connection and a unique cancellation id, with
at most five seconds of cancellation attempts at approximately 100 ms intervals;
an unresponsive cancellation call is bounded by that same deadline. A withdrawn
prompt still waits for the check's actual return before the daemon may start
another prompt, and can only decline even after withdrawal stops retrying.

Host-neutral unit tests cover the policy defaults/owner, literal template/detail
split and both result-mapping phases. The root-only `linux_polkit` integration
test drives real polkitd and pkttyagent through a pty: right/wrong passwords, no
agent, withdrawal with an observed agent cancellation, literal `$(x)` text,
missing setup, and a session-less refusal with the unmodified production policy.
`SUBC_REQUIRE_POLKIT_TEST=1` makes missing prerequisites fail rather than skip;
CI creates a disposable password user and supplies `SUBC_POLKIT_TEST_USER` and
`SUBC_POLKIT_TEST_PASSWORD`. Headless CI temporarily installs a rule restricted
to that exact action and numeric uid, returning only `AUTH_SELF`, to reach the
password cases without an active desktop. The fixture removes the rule and action
on exit. This rule is not part of setup or the shipped policy.

**CI does not verify graphical dialogs, desktop agent rendering, or real logind
Display-session selection.** A person must separately run the example above on
real GNOME and KDE desktops from the daemon's user-service context. Record the
desktop, polkit/agent versions, exact rendered sentence (including `$(x)`),
approve/cancel outcomes, and withdrawal while the dialog is visible. Do not infer
graphical text visibility or prompt dismissal from the terminal-agent test.
