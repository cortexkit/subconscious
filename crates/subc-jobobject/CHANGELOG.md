# Changelog

## 0.2.2

- Add `ensure_root_job`, which places the calling process in a Windows job object that ends every process created afterwards when the caller exits, however it exits. The caller joins an unnamed non-breakaway job before creating children, retaining its sole non-inheritable handle until process exit. Initialization errors are cached and must prevent module creation. Existing host jobs must permit nested assignment on Windows 8 and later; no breakaway fallback is attempted.
