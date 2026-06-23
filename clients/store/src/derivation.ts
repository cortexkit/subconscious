const FNV_OFFSET_BASIS_64 = 0xcbf29ce484222325n;
const FNV_PRIME_64 = 0x100000001b3n;
const MASK_64 = 0xffffffffffffffffn;

const textEncoder = new TextEncoder();

export function postgresDatabaseName(moduleId: string): string {
  return `cortexkit_${postgresSlug(moduleId)}_${fnv1a64Hex(moduleId)}`;
}

export function sqliteStorePath(dataHome: string, moduleId: string): string {
  return `${dataHome.replace(/\/+$/, "")}/cortexkit/${moduleId}/store.db`;
}

function postgresSlug(moduleId: string): string {
  return Array.from(moduleId, (char) => {
    if (isAsciiAlphanumeric(char)) {
      return char.toLowerCase();
    }
    return "_";
  })
    .slice(0, 36)
    .join("");
}

function isAsciiAlphanumeric(char: string): boolean {
  const code = char.charCodeAt(0);
  return (code >= 48 && code <= 57) || (code >= 65 && code <= 90) || (code >= 97 && code <= 122);
}

function fnv1a64Hex(value: string): string {
  let hash = FNV_OFFSET_BASIS_64;

  for (const byte of textEncoder.encode(value)) {
    hash ^= BigInt(byte);
    hash = (hash * FNV_PRIME_64) & MASK_64;
  }

  return hash.toString(16).padStart(16, "0");
}
