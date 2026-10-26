# Device capabilities PUT replies v1

`capabilities-put-replies.json` contains the account service's real replies for
`/v1/device/capabilities`, copied byte for byte. There is no generator here.
Each of the 16 cases records its name, request, and the reply's status,
content-type and exact body.

The `vectors` job in `.github/workflows/ci.yml` discovers this directory and
checks `SHA256SUMS` without running a generator.

Verify the vendored bytes from this directory with:

```sh
sha256sum -c SHA256SUMS
```
