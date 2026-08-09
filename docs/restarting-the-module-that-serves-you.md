# Restarting the module that serves your session

## The command that restarts a module cannot survive that module's restart

`ck module restart <id>` run from a session whose transport that same module
serves will report a transport error, every time, and the restart will have
succeeded anyway.

The mechanism is not subtle once seen. Harness tool calls are executed *by* a
module — for a shell tool served by aft, the process tree is:

```
bash → sh → aft-subc → ck-subc → systemd
```

so the command's own output travels back over a subc route that aft serves.
Restarting aft tears down every aft-bound route, including that one. The reply
path dies before the reply arrives.

**This is structural. No client fix can make that particular invocation report
cleanly** — the route carrying the answer is the route being torn down. Do not
file it as a bug against the CLI, and do not expect a future version to fix it.
What can improve is how the error reads, not whether it happens.

## The outcome is UNKNOWN, not failed

This is the part that costs real work if you get it wrong.

A restart drains for a bounded window and then tears down regardless of whether
in-flight requests finished (`crates/subc-core/src/supervise.rs`, in
`begin_forwarding_drain_with` — the quiescence result is consumed only by a
`warn!`; route release and GOODBYE emission run unconditionally afterwards). So
a command that was in flight when the route died **may have executed
completely**, with only its response lost.

Observed directly: a `ck module rescan && ck module restart aft` chain returned
only the transport error, with the rescan's stdout — which had already printed —
gone from the buffer too. Whole-buffer loss of the pending call. The rescan had
run.

Consequences:

- **Never blind-retry a mutation** after this error. Re-running a `git push`, a
  migration, or an `rm` because the transport lost the answer is how one
  completed operation becomes two.
- **Verify state, don't re-run.** `ck module health` and `ck module status <id>`
  go over a fresh connection and will tell you what actually happened.
- A wrapper script that treats a nonzero exit here as "the restart failed" will
  classify a successful restart as a failure. If you automate around
  `ck module restart`, check module state afterwards rather than trusting the
  exit line.

## Only the restarted module's routes die

The teardown is scoped to the draining module's endpoint
(`release_module_endpoint_routes`), so restarting a module that does *not* serve
your session is uneventful.

Verified as a control: a `stop claustrum && ck-auth import && start claustrum &&
health` chain run from an aft-served session returned the complete buffer —
both ack tables, the import line, and the trailing health read — with no
transport error. Same operator, same wrapper, same daemon, different module.

So the rule is about *whose* transport you are cutting, not about restarts in
general.

## Avoiding it

Run the restart from somewhere the module does not serve — a plain terminal, or
a session backed by a different module — and the ack table prints normally.

If you must restart from a session that module serves, expect the error, and
confirm with `ck module health` rather than reading anything into the exit line.
