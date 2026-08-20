const FNV_OFFSET_BASIS_64 = 0xcbf29ce484222325n;
const FNV_PRIME_64 = 0x100000001b3n;
const MASK_64 = 0xffffffffffffffffn;

const textEncoder = new TextEncoder();

export function postgresDatabaseName(moduleId: string): string {
  return `cortexkit_${postgresSlug(moduleId)}_${fnv1a64Hex(moduleId)}`;
}

/**
 * The platform data home: `$XDG_DATA_HOME` if set and absolute, else
 * `$HOME/.local/share`. THE definition — byte-parity with Rust
 * `cortexkit-store-types::resolve_data_home`. Modules must not re-derive it by
 * hand: hand-rolled env-or-XDG-or-HOME assembly is how a module directory got
 * fed back in as a data home and doubled a production store path
 * (`<module>/cortexkit/<module>`, astrocyte 2026-08). Relocation is
 * XDG_DATA_HOME only; private `*_DATA_DIR` conventions are unsupported.
 */
export function resolveDataHome(env: Record<string, string | undefined> = processEnv()): string {
  const xdg = env.XDG_DATA_HOME;
  if (xdg && xdg.startsWith("/")) return xdg.replace(/\/+$/, "");
  const home = (env.HOME ?? "~").replace(/\/+$/, "");
  return `${home}/.local/share`;
}

/** The conventional module data directory for non-sqlite state (journals, caches). */
export function moduleDataDir(moduleId: string, env?: Record<string, string | undefined>): string {
  assertPathSafeModuleId(moduleId);
  return `${resolveDataHome(env)}/cortexkit/${moduleId}`;
}

/**
 * THE entry point for a module resolving its own store path. Wraps
 * resolveDataHome + sqliteStorePath so no caller hand-assembles either half;
 * the two-argument sqliteStorePath below exists for callers holding a
 * genuinely foreign data home (daemon descriptor resolution, rigs, tests).
 */
export function moduleStorePath(moduleId: string, env?: Record<string, string | undefined>): string {
  return sqliteStorePath(resolveDataHome(env), moduleId);
}

function processEnv(): Record<string, string | undefined> {
  return (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};
}

export function sqliteStorePath(dataHome: string, moduleId: string): string {
  // The first argument is an XDG-STYLE DATA HOME (`~/.local/share`), never an
  // already-qualified module directory — passing `<dataHome>/cortexkit/<id>`
  // here doubles the nesting. Modules resolving their OWN path call
  // moduleStorePath above.
  // REFUSE rather than sanitize (issue #32): this derivation must stay
  // byte-identical to the daemon's Rust derivation (subc-core
  // `StorageConfig::descriptor_for`) and to every store already on disk, so
  // mapping unsafe characters the way `postgresDatabaseName` does would
  // silently re-path deployed stores and desynchronize the two languages.
  // Refusal changes nothing for any id that ever worked; what it removes is
  // the id-as-path primitive: `../` escaping `${dataHome}/cortexkit/`, and
  // `a/b` vs `a//b` -- distinct ids, one POSIX file -- silently sharing a
  // store. The daemon enforces the same predicate at registration; this is
  // the standalone-consumer half of the same boundary.
  assertPathSafeModuleId(moduleId);
  return `${dataHome.replace(/\/+$/, "")}/cortexkit/${moduleId}/store.db`;
}

function assertPathSafeModuleId(moduleId: string): void {
  const refuse = (reason: string) => {
    throw new TypeError(
      `module_id ${JSON.stringify(moduleId)} is not usable as a path component: ${reason}`,
    );
  };
  if (moduleId.length === 0) refuse("empty");
  if (moduleId.includes("/") || moduleId.includes("\\")) refuse("contains a path separator");
  if (moduleId === "." || moduleId === "..") refuse("is a dot path component");
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(moduleId)) refuse("contains a control character");
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
