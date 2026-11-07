# Ask attachments v1

Tag: `ask-attachments-v1`. Photos the person attaches to an ask's answer or to a
clarification.

`ask-read-both.json` is the producer's `ask.get` reply for one ask that has both:
a clarification carrying one WebP photo, then an answer carrying a PNG and a
JPEG. The producer captured it through its real handler, and the file is copied
here byte for byte. Generated ids are replaced by angle-bracketed markers, and
the times are fixed.

The fields it pins:

- `answerArtifactIds` and `answerAttachments` on the ask: the answer's photos,
  in the order they were sent.
- `artifactIds` and `attachments` on a thread entry: the clarification's photos.
- Each pointer has `{artifactId, title, mime, byteCount, kind, sealed}`. The
  ask-wide `attachments` list also holds every one of them.

`SHA256SUMS` holds the producer's own digest line for this file, so
`shasum -a 256 -c SHA256SUMS` in this directory proves the copy is unchanged.
Never edit the JSON by hand: a changed sample comes from the producer, with its
new digest.
