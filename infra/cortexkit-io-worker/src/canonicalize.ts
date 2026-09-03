/**
 * Signatures cover exact bytes, so key order and whitespace must be stable
 * across isolates. Recursive sort + JSON.stringify (no space argument) is
 * that document.
 */
export function canonicalize(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === "object") {
    const input = value as Record<string, unknown>;
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(input).sort()) {
      out[key] = sortKeys(input[key]);
    }
    return out;
  }
  return value;
}
