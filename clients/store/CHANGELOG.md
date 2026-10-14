# Changelog

## 0.2.3

- Use the shared POSIX ancestor policy from `@cortexkit/private-files` 0.2.0, preserving the existing public API and refusal messages. Ubuntu's umask-0002 config layout remains accepted when its user-owned directory's effective group has no other members and no other users sharing it as their primary group.
- POSIX now requires the `private-files` JavaScript package to be installed because it owns the single policy implementation; store no longer runs with that dependency absent. Native prebuilds remain unnecessary on POSIX and are never loaded by its checks.

## 0.2.2

- `createPrivateDirectory` can create a private directory directly inside a
  root-owned sticky folder such as `/tmp`. It used to refuse with "sticky
  ancestor without an owner-only child" before creating the very owner-only
  child the rule asks for. The new directory is checked with the same policy
  after creation and removed if it fails. A sticky folder higher up still
  needs an existing owner-only directory below it, and a file directly inside
  a sticky folder is still refused, as `checkOwnerOnlyPath` refuses it.

## 0.2.1

- Publish the 0.2.0 changes below. The `0.2.0` on npm was built from an earlier commit: it checks Windows paths with an AppData location rule and doesn't use @cortexkit/private-files. Use 0.2.1 or later.

## 0.2.0

- Add `checkOwnerOnlyPath` and `checkOwnerOnlyPathAsync` to check secret-storage paths without creating files or directories. Unix checks resolved ancestors, including sticky-directory and user-private-group exceptions, unchanged. Windows uses a verified owner-only security descriptor via @cortexkit/private-files instead of an AppData location rule.
- Behaviour change: synchronous `checkOwnerOnlyPath` always refuses on Windows and names `checkOwnerOnlyPathAsync`, because native verification is asynchronous. A missing addon (`PrivateFilesUnavailableError`) refuses; there is no location or unchecked-write fallback.
- Add exclusive `createPrivateFile` and single-directory `createPrivateDirectory` helpers. Windows delegates secure creation and same-handle writing to private-files; Unix checks the parent with the existing policy before `0600`/`0700` creation.
- Depend on `@cortexkit/private-files` `^0.1.0`, linked to the sibling package in the development lockfile before its first publish. The package and its native addon are loaded only for Windows operations. Platform, identity, and filesystem injection remain available for portable permission tests; the legacy environment option no longer grants trust.

## 0.1.3

- Publish compiled ESM and type declarations so Node 18+ and Bun can import the package without a TypeScript loader.
