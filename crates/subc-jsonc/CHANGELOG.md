# Changelog

## 0.1.5

- Edits address the last of repeated keys, the copy JSON readers keep, instead of the first.
  Removing a key written more than once is refused, because it would expose an older copy.

## 0.1.4

- Add atomic batches of JSONC edits that can rename a member or wrap the root object without losing comments.
