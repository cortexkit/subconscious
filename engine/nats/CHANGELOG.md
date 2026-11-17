# Changelog

## 0.1.2

- Start hub instances only after the listener state arrives, with TLS 1.3 and an
  Ed25519 signer over the private channel. Refuse listener changes until restart
  and include listener binding in engine status.
- Leave hub configuration unread while idle, and report missing or invalid
  configuration as a refused state so hub setup can finish without restarting.

## 0.1.1

- Add a side-effect-free `--version` command.

## 0.1.0

- Embed the pinned NATS server with channel-backed leaf signing and remote reload.
