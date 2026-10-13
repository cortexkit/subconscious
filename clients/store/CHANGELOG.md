# Changelog

## 0.2.1

- Publish the 0.2.0 changes below. The `0.2.0` on npm was built from an earlier commit: it checks Windows paths with an AppData location rule and doesn't use @cortexkit/private-files. Use 0.2.1 or later.

## 0.2.0

- Add `checkOwnerOnlyPath` and `checkOwnerOnlyPathAsync` to check secret-storage paths without creating files or directories. Unix checks resolved ancestors, including sticky-directory and user-private-group exceptions, unchanged. Windows uses a verified owner-only security descriptor via @cortexkit/private-files instead of an AppData location rule.
- Behaviour change: synchronous `checkOwnerOnlyPath` always refuses on Windows and names `checkOwnerOnlyPathAsync`, because native verification is asynchronous. A missing addon (`PrivateFilesUnavailableError`) refuses; there is no location or unchecked-write fallback.
- Add exclusive `createPrivateFile` and single-directory `createPrivateDirectory` helpers. Windows delegates secure creation and same-handle writing to private-files; Unix checks the parent with the existing policy before `0600`/`0700` creation.
- Depend on `@cortexkit/private-files` `^0.1.0`, linked to the sibling package in the development lockfile before its first publish. The package and its native addon are loaded only for Windows operations. Platform, identity, and filesystem injection remain available for portable permission tests; the legacy environment option no longer grants trust.

## 0.1.3

- Publish compiled ESM and type declarations so Node 18+ and Bun can import the package without a TypeScript loader.
