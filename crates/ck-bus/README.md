# ck-bus

The supervised owner of the CortexKit NATS message plane (module id `ckbus`). The build
specification is `docs/specs/ck-bus-module.md`; keys and first boot follow
`docs/designs/nats-install-trust-chain.md`.

## Supervised environment

Set in the `env` block of the `ckbus` module declaration. None has a built-in default
path; a missing broker input answers health down with cause `broker-config-absent`.

| Variable | Meaning |
| --- | --- |
| `CKBUS_NATS_URL` | The local `nats-server` client URL, `nats://<loopback host>:<port>`. A non-loopback host is refused: the listener has no TLS. |
| `CKBUS_OPERATOR_JWT` | Path of the root-signed operator JWT `ck setup` wrote. Its `signing_keys` must list the operator signer (`signing:ck-bus-operator-signer:1`). |
| `CKBUS_SYSTEM_ACCOUNT` | The system account id (`A...`); must equal the operator JWT's `system_account`. |
| `CKBUS_SENTINEL_PERIOD_MS`, `CKBUS_SENTINEL_TIMEOUT_MS` | Test seam for the sentinel period and timeout (defaults 10000 and 2000). Bootstrap retries once per period. |
| `XDG_DATA_HOME` / `HOME` | Store root: `cortexkit_store_types::module_data_dir("ckbus")`. |

`SUBC_MODULE_ID`, `SUBC_LAUNCH_NONCE` and `--subc <connection file>` come from the
daemon.

## Server requirements

The local server runs in operator mode with the full (directory) resolver, the system
account preloaded, a loopback listener (never `0.0.0.0`), and `max_control_line`
above the 4 KiB default, because a CONNECT carrying ck-bus's box-user grant is longer.
