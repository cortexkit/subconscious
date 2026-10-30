# Changelog

## 0.2.3

- Add query-only job handles, each open until it's dropped, for reporting the working sets and count of live member processes. Enumeration retries a growing buffer and checks membership through each opened process handle. Dropping the job owner still ends its processes even while a reader holds a query handle.

## 0.2.2

- Add `ensure_root_job`, which places the calling process in a Windows job object that ends every process created afterwards when the caller exits, however it exits. The caller joins an unnamed non-breakaway job before creating children, retaining its sole non-inheritable handle until process exit. Initialization errors are cached and must prevent module creation. Existing host jobs must permit nested assignment on Windows 8 and later; no breakaway fallback is attempted.
