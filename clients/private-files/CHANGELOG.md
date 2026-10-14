# Changelog

## 0.2.1

- Allow `securePrivatePath` on Windows objects owned by the current user, Administrators or LocalSystem, matching the trusted-owner policy for private-file reads and verification. Replace only the DACL through the pinned handle, preserving the owner; other owners remain refused with `wrong-owner` and their SID.

## 0.2.0

- Add `verifySafeParent` and its POSIX synchronous form for directories holding secret files. Windows checks trusted owners, effective mutation rights, reparse points and pinned ancestors without imposing the secret-file DACL template.
- Export the shared POSIX ancestor policy for sync and async callers. It permits a root-owned sticky directory only with an owner-only immediate child, and a group-writable user directory only when account files prove that its effective group has no other members or other primary-group users. Export `WRITABLE_BY_OTHERS` for unsafe entry mutation rights.
- Keep secret-file creation, owner-only verification and reads unchanged.
