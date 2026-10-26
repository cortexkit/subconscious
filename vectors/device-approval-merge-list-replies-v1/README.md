# Device-approval merge list replies v1

`merge-list-replies.json` contains the account service's real replies for the
held account-merge list routes `GET /v1/device/merges` (phones and other
rendezvous devices) and `GET /v1/machine/merges` (computers), copied byte for
byte. There is no generator here.

Each of the 6 cases records its route, case name (`two-item`, `empty` or
`non-enforced`), HTTP status and exact reply bytes. Both routes share one reply
shape, `{"merges":[<MergeView>],"cursor":null}`.

The `vectors` job in `.github/workflows/ci.yml` discovers this directory and
checks `SHA256SUMS` without running a generator. Swift tests also pin the digest.

Verify the vendored bytes from this directory with:

```sh
sha256sum -c SHA256SUMS
```
