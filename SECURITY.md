# Security

## Reporting a vulnerability

Report vulnerabilities privately. Do not open a public issue, pull request or
discussion for a suspected vulnerability. Include the affected version
(`ck --version`, `ck daemon`), the platform, and the steps that reproduce it.

<!-- reporting channel: pending owner decision -->
Private reporting through GitHub will be enabled for this repository; until then, contact the maintainers through the address on the CortexKit organization profile.

## Threat model

subconscious runs as an ordinary process in your user account and serves only
the local machine. This section states what it protects, what it does not, and
which boundary actually separates one party from another.

### The boundary is the operating-system account

Every process running as your user can do what you can: read your files,
including the daemon's connection file, and inspect other processes you own.
The daemon does not and cannot defend against code running as the same user as
the daemon. If you run an agent whose actions you do not trust, run it under a
separate operating-system account. The connection file that holds the key is
readable only by the daemon's account, so an agent in another account cannot
authenticate to the daemon or reach its modules. The daemon does not defend
against an administrator or `root`.

### What protects against other accounts

- The daemon listens only on loopback (`127.0.0.1`, and `::1` when available).
- The connection file, which holds the key, is written owner-only (`0600` on
  Unix, a private DACL on Windows), and readers refuse one that is not. Tests:
  `read_rejects_group_or_world_readable_file` (`crates/subc-transport/src/connection_file.rs`) and
  `owner_only_file_validation_refuses_a_different_effective_uid` (`crates/subc-transport/src/connection_file.rs`).
- The writer refuses to publish the key beneath any directory another user can
  write (sticky directories such as `/tmp` excepted), so another account cannot
  swap the file for its own. Test:
  `a_group_writable_ancestor_refuses_and_the_same_tree_without_the_bit_publishes` (`crates/subc-transport/src/connection_file.rs`).
- The run directory (`<data home>/cortexkit/run`) is created `0700`, and
  tightened at start if it is wider, so other accounts cannot list which
  modules are running. Tightening is best effort: a directory the daemon cannot
  change is logged and the daemon still starts. Test:
  `run_dir_is_created_private_and_an_inherited_wide_one_is_tightened` (`crates/subc-daemon/src/daemon_config.rs`).

### The handshake

Every connection starts with a mutual HMAC-SHA256 challenge over the 32-byte
key in the connection file, with fresh nonces from both sides and the daemon's
id bound into each proof (`crates/subc-transport/src/auth.rs`).

It proves that both ends hold the key, so a client cannot be fooled by another
listener on the same port, and an old key stops working when the daemon
restarts with a new one. The handshake has one overall deadline and a
4096-byte message cap, so an unauthenticated peer cannot hold a connection
open or force a large allocation. Tests:
`foreign_server_reused_port_never_receives_client_auth` (`crates/subc-transport/tests/handshake.rs`),
`bad_client_key_is_rejected_by_server` (`crates/subc-transport/tests/handshake.rs`),
`auth_deadline_closes_idle_client` (`crates/subc-transport/tests/handshake.rs`),
`oversize_pre_auth_message_is_rejected_before_body_allocation` (`crates/subc-transport/tests/handshake.rs`).

It does not prove who the peer is. Any process that can read the connection
file passes it. The `role` a client sends during the handshake is not checked
and carries no authority.

### Launch nonces and the principal

When the daemon starts a module it gives that process a one-time launch nonce.
On macOS and Linux the nonce arrives over an inherited pipe rather than the
environment, because other processes of the same user can read a process's
environment. On Windows it arrives over a named pipe that serves only that
child process; while modules move to the pipe, the daemon also sets it in the
child's environment.

A module presents its nonce to register a `reserved` module id, and when it
opens a route as a client. On each route, the daemon stamps the target's
`route.bind` with a principal: `reserved` with the module id when the opener
proved a nonce the daemon minted, `direct` when it presented none. A module
grants privilege only on the principal. Everything else on the bind, such as
`project_root`, `harness` and `session`, is chosen by the caller.

This stops a process that holds the key but not the nonce from registering
under a reserved id or from opening routes as a supervised module. It does not
stop a same-user process that can read another process's memory. On macOS,
modules signed with the hardened runtime and without the `get-task-allow`
entitlement refuse debugger attachment from same-user processes; see
[`docs/launch-nonce.md`](docs/launch-nonce.md).

### Scopes and bind proofs

A module that owns a scope may attach the SHA-256 of a 32-byte secret to it.
The daemon then admits a route under that scope only from an opener that
presents the secret, the owner included, and compares digests in constant
time. The daemon stores only the hash; the secret never appears in logs,
refusal details, `scope.describe` or bind stamps. A bind proof limits who can
use a scope to whoever holds its secret. It does not protect a secret that a
same-user process can read from the holder's memory or files. See
[`docs/scopes.md`](docs/scopes.md).

### Operator approval

With `privileged_operator_prompt: "enforce"` in `subc.jsonc`, the daemon asks
the person at the machine to approve each restart, swap, reload, stop, start,
rescan and reserved-id release before acting, naming the operation and the
requesting process. The default, `"log"`, records that a prompt would have been
shown and proceeds. Modules may also ask the person to confirm a write with
`operator.confirm`. Approval protects against an action nobody intended; it
does not protect against a same-user process that can also answer or suppress
the prompt.

### Guarantees and non-guarantees

The daemon guarantees:

- No other non-administrator account on the machine can read the connection
  key or authenticate.
- A connection that does not hold the current key is refused before any
  envelope frame is read.
- A `reserved` principal on a bind means the opener proved a launch nonce the
  daemon issued to a process it spawned.
- A scoped route whose scope carries a bind-proof hash was opened by a holder
  of the matching secret.

The daemon does not guarantee:

- Any separation between processes running as the same user.
- That caller-supplied identity fields (`BindIdentity`) are true.
- Confidentiality of traffic from a process that can read the daemon's or a
  module's memory.
- Integrity of a module binary placed by someone who can write your files.

### Modules and third-party code

Modules run as your user with your permissions. A third-party stdio MCP server
run through `ck-mcp-stdio-adapter` is a child process of that adapter and has
no sandbox of its own; see [`docs/mcp-stdio-adapter.md`](docs/mcp-stdio-adapter.md).
Only install modules and MCP servers you would run directly.
