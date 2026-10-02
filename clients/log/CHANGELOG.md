# Changelog

## 0.2.3

- Publish compiled ESM and type declarations, and depend on the published `@cortexkit/store` version instead of a sibling-directory path. Node 18+ and Bun can import the packed packages.
- Parse literal quotes and plain trailing brackets in free-text messages without rejecting the line. Rendered bytes and the documented ambiguity of trailing `key=value` message text are unchanged.

## 0.2.2

- Stop credential query-value redaction at closing parentheses and brackets so wrapped URLs retain the diagnostic text that follows them.
