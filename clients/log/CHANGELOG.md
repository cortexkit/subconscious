# Changelog

## 0.2.3

- Publish compiled ESM and type declarations, and depend on the published `@cortexkit/store` version instead of a sibling-directory path. Node 18+ and Bun can import the packed packages.

## 0.2.2

- Stop credential query-value redaction at closing parentheses and brackets so wrapped URLs retain the diagnostic text that follows them.
