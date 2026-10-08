# subc-presence

An OS-binding leaf crate used only by the daemon's per-target macOS/Windows
dependencies. It denies unsafe code by default and enables it only on individual
FFI items. The daemon owns authorization, deadlines, cancellation reasons and
audit; this crate only supplies a blocking prompt and a cross-thread withdraw
handle. An explicit successful authentication is the only approving outcome.

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
- Other platforms return `UnsupportedPlatform` without publishing a handle. The
  Linux daemon keeps its inline `unsupported_platform` provider and does not
  depend on this crate: an agent displaying only a fixed policy message cannot
  show the person the requested write.

## Person-present release checks

These checks cannot be inferred from unit tests. In particular, **Windows
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
