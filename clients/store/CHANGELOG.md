# Changelog

## 0.2.0

- Add `checkOwnerOnlyPath` and `checkOwnerOnlyPathAsync` to check secret-storage paths without creating files or directories. Unix checks resolved ancestors, including sticky-directory and user-private-group exceptions; Windows requires resolved containment in the user's Local or Roaming app-data folders.
- Expose platform, environment, identity, and filesystem injection for portable permission tests. Windows containment cannot detect deliberately loosened ACLs; Rust callers should use `subc-os` DACL verification.

## 0.1.3

- Publish compiled ESM and type declarations so Node 18+ and Bun can import the package without a TypeScript loader.
