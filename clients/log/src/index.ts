import * as fs from "node:fs";
import * as path from "node:path";

import { moduleDataDir } from "@cortexkit/store";

export type Level = "error" | "warn" | "info" | "debug" | "trace";

export interface Session {
  issuer: string;
  id: string;
}

export type FieldValue = string | number | boolean;

export interface LogEvent {
  at_ms: number;
  level: Level;
  module: string;
  session: Session | null;
  tag: string | null;
  message: string;
  fields: readonly (readonly [string, FieldValue])[];
}

export interface ParsedLine {
  at_ms: number;
  timestamp: string;
  level: Level;
  module: string;
  session: Session | null;
  tag: string | null;
  message: string;
  fields: Array<[string, string]>;
}

export interface LogConfig {
  moduleId: string;
  lane:
    | { kind: "module" }
    | { kind: "plugin"; harness: string }
    | { kind: "custom"; path: string };
  spec?: string;
  retention?: { maxFileMb: number; keep: number; maxAgeDays: number };
  redactor?: (line: string) => string;
  clock?: () => number;
  tags: readonly string[];
}

export interface LoggerStats {
  swallowedWrites: number;
  fallbackActive: boolean;
  path: string;
  ansiStripped: number;
}

export interface Logger {
  error(
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void;
  warn(
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void;
  info(
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void;
  debug(
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void;
  trace(
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void;
  withSession(issuer: string, id: string): Logger;
  enabled(level: Level, tag?: string): boolean;
  stats(): LoggerStats;
  flush(): Promise<void>;
  close(): Promise<void>;
}

const LEVELS: readonly Level[] = ["error", "warn", "info", "debug", "trace"];
const LEVEL_RANK: Record<Level, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
  trace: 4,
};
const DAY_MS = 24 * 60 * 60 * 1_000;
const REDACTED = "[REDACTED]";

let malformedSpecReported = false;
let writeFailureReported = false;

interface FormatResult {
  line: string;
  ansiStripped: number;
}

interface ParsedFilter {
  defaultLevel: Level | "off";
  tags: Map<string, Level | "off">;
}

interface RuntimeRetention {
  maxFileBytes: number;
  keep: number;
  maxAgeDays: number;
}

function validateToken(label: string, value: string): void {
  if (value.length === 0 || /\s/.test(value)) {
    throw new Error(`${label} must be a non-empty token without whitespace`);
  }
}

function escapeMessage(value: string): string {
  return value.replaceAll("\\", "\\\\").replaceAll("\r", "\\r").replaceAll("\n", "\\n");
}

function formatValue(value: FieldValue): string {
  if (typeof value !== "string") return String(value);
  if (value !== "" && !/[\s"]/.test(value)) return value;
  return `"${value
    .replaceAll("\\", "\\\\")
    .replaceAll("\r", "\\r")
    .replaceAll("\n", "\\n")
    .replaceAll('"', '\\"')}"`;
}

function stripAnsi(value: string): { value: string; count: number } {
  let count = 0;
  const stripped = value.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, () => {
    count += 1;
    return "";
  });
  return { value: stripped, count };
}

function formatLineWithStats(event: LogEvent): FormatResult {
  if (!LEVELS.includes(event.level)) throw new Error(`unknown log level: ${String(event.level)}`);
  validateToken("module", event.module);
  if (event.session) {
    validateToken("session issuer", event.session.issuer);
    validateToken("session id", event.session.id);
  }
  if (event.tag !== null) validateToken("tag", event.tag);

  const timestamp = new Date(event.at_ms).toISOString();
  const columns = [
    timestamp,
    event.level.toUpperCase().padEnd(5, " "),
    event.module,
  ];
  if (event.session) columns.push(`session=${event.session.issuer}:${event.session.id}`);
  if (event.tag !== null) columns.push(`tag=${event.tag}`);
  columns.push(escapeMessage(event.message));

  for (const [key, value] of event.fields) {
    if (key.length === 0 || /[\s=]/.test(key)) {
      throw new Error("log field keys must be non-empty tokens without whitespace or '='");
    }
    columns.push(`${key}=${formatValue(value)}`);
  }

  const stripped = stripAnsi(columns.join(" "));
  return { line: stripped.value, ansiStripped: stripped.count };
}

/** Render one canonical fleet log line without a trailing newline. */
export function formatLine(event: LogEvent): string {
  return formatLineWithStats(event).line;
}

function tokenizeColumns(input: string): string[] | null {
  const tokens: string[] = [];
  let token = "";
  let quoted = false;
  let escaped = false;

  for (const char of input) {
    if (escaped) {
      token += char;
      escaped = false;
      continue;
    }
    if (quoted && char === "\\") {
      token += char;
      escaped = true;
      continue;
    }
    if (char === '"') {
      quoted = !quoted;
      token += char;
      continue;
    }
    if (char === " " && !quoted) {
      if (token.length > 0) {
        tokens.push(token);
        token = "";
      }
      continue;
    }
    token += char;
  }

  if (quoted || escaped) return null;
  if (token.length > 0) tokens.push(token);
  return tokens;
}

function decodeEscapedText(value: string): string | null {
  let decoded = "";
  for (let index = 0; index < value.length; index += 1) {
    const char = value[index];
    if (char !== "\\") {
      decoded += char;
      continue;
    }
    index += 1;
    if (index >= value.length) return null;
    const escaped = value[index];
    if (escaped === "n") decoded += "\n";
    else if (escaped === "r") decoded += "\r";
    else if (escaped === '"' || escaped === "\\") decoded += escaped;
    else decoded += escaped;
  }
  return decoded;
}

function decodeFieldToken(token: string): [string, string] | null {
  const equals = token.indexOf("=");
  if (equals <= 0) return null;
  const key = token.slice(0, equals);
  if (/[\s=]/.test(key)) return null;
  const rawValue = token.slice(equals + 1);
  if (!rawValue.startsWith('"')) return rawValue.includes('"') ? null : [key, rawValue];
  if (rawValue.length < 2 || !rawValue.endsWith('"')) return null;

  const value = decodeEscapedText(rawValue.slice(1, -1));
  return value === null ? null : [key, value];
}

/** Parse a canonical fleet line, returning a stable reason for malformed input. */
export function parseLine(line: string): ParsedLine | { reject: string } {
  if (/\x1b\[/.test(line)) return { reject: "ansi_forbidden" };

  const timestampEnd = line.indexOf(" ");
  if (timestampEnd < 0) return { reject: "timestamp_not_utc_z" };
  const timestamp = line.slice(0, timestampEnd);
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(timestamp)) {
    return { reject: "timestamp_not_utc_z" };
  }
  const atMs = Date.parse(timestamp);
  if (!Number.isFinite(atMs) || new Date(atMs).toISOString() !== timestamp) {
    return { reject: "timestamp_invalid" };
  }

  const afterTimestamp = line.slice(timestampEnd + 1);
  const levelColumn = afterTimestamp.slice(0, 5);
  const levelText = levelColumn.trim().toLowerCase();
  if (
    afterTimestamp.length < 7 ||
    afterTimestamp[5] !== " " ||
    !LEVELS.includes(levelText as Level) ||
    levelColumn !== levelText.toUpperCase().padEnd(5, " ")
  ) {
    return { reject: "level_column_width" };
  }

  const afterLevel = afterTimestamp.slice(6);
  const moduleEnd = afterLevel.indexOf(" ");
  if (moduleEnd <= 0) return { reject: "module_missing" };
  const module = afterLevel.slice(0, moduleEnd);
  const tokens = tokenizeColumns(afterLevel.slice(moduleEnd + 1));
  if (!tokens || tokens.length === 0) return { reject: "message_missing" };

  let cursor = 0;
  let session: Session | null = null;
  let tag: string | null = null;
  const sessionToken = tokens[cursor];
  if (sessionToken?.startsWith("session=")) {
    const sessionValue = sessionToken.slice("session=".length);
    const separator = sessionValue.lastIndexOf(":");
    if (separator <= 0 || separator === sessionValue.length - 1) {
      return { reject: "session_missing_issuer" };
    }
    session = {
      issuer: sessionValue.slice(0, separator),
      id: sessionValue.slice(separator + 1),
    };
    cursor += 1;
  }
  const tagToken = tokens[cursor];
  if (tagToken?.startsWith("tag=")) {
    tag = tagToken.slice("tag=".length);
    if (tag.length === 0) return { reject: "tag_missing" };
    cursor += 1;
  }

  const remaining = tokens.slice(cursor);
  let fieldsStart = remaining.length;
  for (let index = 0; index < remaining.length; index += 1) {
    if (remaining.slice(index).every((token) => decodeFieldToken(token) !== null)) {
      fieldsStart = index;
      break;
    }
  }
  if (fieldsStart === 0) return { reject: "message_missing" };

  const fields = remaining.slice(fieldsStart).map((token) => decodeFieldToken(token)!);
  const message = decodeEscapedText(remaining.slice(0, fieldsStart).join(" "));
  if (message === null) return { reject: "message_escape_invalid" };

  return {
    at_ms: atMs,
    timestamp,
    level: levelText as Level,
    module,
    session,
    tag,
    message,
    fields,
  };
}

function reportStderr(message: string): void {
  try {
    process.stderr.write(`${message}\n`);
  } catch {
    // Logging failures must never escape into the module using the logger.
  }
}

function parseFilter(spec: string, declaredTags: ReadonlySet<string>): ParsedFilter {
  const fallback = (): ParsedFilter => ({ defaultLevel: "info", tags: new Map() });
  if (spec.trim() === "") return fallback();

  let defaultLevel: Level | "off" = "info";
  const tags = new Map<string, Level | "off">();
  let malformed = false;

  for (const rawDirective of spec.split(",")) {
    const directive = rawDirective.trim();
    if (directive === "") {
      malformed = true;
      break;
    }
    const equals = directive.indexOf("=");
    if (equals < 0) {
      if (directive === "off" || LEVELS.includes(directive as Level)) {
        defaultLevel = directive as Level | "off";
      }
      continue;
    }
    if (equals === 0 || equals !== directive.lastIndexOf("=") || equals === directive.length - 1) {
      malformed = true;
      break;
    }

    const tag = directive.slice(0, equals);
    const level = directive.slice(equals + 1);
    if (!declaredTags.has(tag)) continue;
    if (level !== "off" && !LEVELS.includes(level as Level)) {
      malformed = true;
      break;
    }
    tags.set(tag, level as Level | "off");
  }

  if (malformed) {
    if (!malformedSpecReported) {
      malformedSpecReported = true;
      reportStderr("@cortexkit/log: malformed CK_LOG; using info");
    }
    return fallback();
  }
  return { defaultLevel, tags };
}

function permits(filterLevel: Level | "off", eventLevel: Level): boolean {
  return filterLevel !== "off" && LEVEL_RANK[eventLevel] <= LEVEL_RANK[filterLevel];
}

/** Redact known credential shapes before optional caller redaction. */
function defaultRedactor(line: string): string {
  return line
    .replace(
      /(Authorization:\s*)(?:"(?:\\.|[^"\r\n])*"|(?:Bearer|Basic)\s+\S+|\S+)/gi,
      `$1${REDACTED}`,
    )
    .replace(/\bBearer\s+[A-Za-z0-9._~+/=-]+/gi, `Bearer ${REDACTED}`)
    .replace(/\beyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/g, REDACTED)
    .replace(/\bckh_[A-Za-z0-9_-]+\b/g, REDACTED)
    .replace(/\bsk-[A-Za-z0-9_-]+\b/g, REDACTED)
    .replace(/\b(?:ghp|gho)_[A-Za-z0-9_-]+\b/g, REDACTED);
}

function resolveLogPath(config: LogConfig): string {
  const dataDir = moduleDataDir(config.moduleId);
  if (config.lane.kind === "custom") return config.lane.path;
  if (config.lane.kind === "module") {
    return path.join(dataDir, "logs", `${config.moduleId}.log`);
  }
  validateToken("plugin harness", config.lane.harness);
  if (config.lane.harness === "." || config.lane.harness === ".." || /[/\\]/.test(config.lane.harness)) {
    throw new Error("plugin harness must not contain path separators");
  }
  return path.join(dataDir, "logs", `${config.moduleId}.${config.lane.harness}.log`);
}

function runtimeRetention(config: LogConfig): RuntimeRetention {
  const configured = config.retention ?? { maxFileMb: 32, keep: 2, maxAgeDays: 14 };
  const hiddenBytes = (configured as typeof configured & { maxFileBytes?: number }).maxFileBytes;
  const maxFileBytes = hiddenBytes ?? configured.maxFileMb * 1024 * 1024;
  if (!Number.isFinite(maxFileBytes) || maxFileBytes <= 0) {
    throw new Error("retention max file size must be positive");
  }
  if (!Number.isInteger(configured.keep) || configured.keep < 0) {
    throw new Error("retention keep must be a non-negative integer");
  }
  if (!Number.isFinite(configured.maxAgeDays) || configured.maxAgeDays < 0) {
    throw new Error("retention maxAgeDays must be non-negative");
  }
  return { maxFileBytes, keep: configured.keep, maxAgeDays: configured.maxAgeDays };
}

class FileSink {
  readonly path: string;
  readonly retention: RuntimeRetention;
  readonly clock: () => number;
  readonly callerRedactor?: (line: string) => string;
  swallowedWrites = 0;
  fallbackActive = false;
  ansiStripped = 0;
  private fd: number | null = null;
  private bytesWritten = 0;
  private closed = false;

  constructor(config: LogConfig) {
    this.path = resolveLogPath(config);
    this.retention = runtimeRetention(config);
    this.clock = config.clock ?? Date.now;
    this.callerRedactor = config.redactor;

    try {
      const directory = path.dirname(this.path);
      fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
      if (process.platform !== "win32") fs.chmodSync(directory, 0o700);
      this.pruneGenerations(this.clock());
      this.openActive();
    } catch (error) {
      this.fallbackActive = true;
      reportStderr(
        `@cortexkit/log: cannot open ${this.path}; falling back to stderr: ${String(error)}`,
      );
    }
  }

  write(line: string, now: number, ansiCount: number): void {
    this.ansiStripped += ansiCount;
    try {
      let redacted = defaultRedactor(line);
      if (this.callerRedactor) redacted = this.callerRedactor(redacted);
      const clean = stripAnsi(redacted);
      this.ansiStripped += clean.count;
      const output = `${clean.value}\n`;

      if (this.fallbackActive) {
        process.stderr.write(output);
        return;
      }
      if (this.closed || this.fd === null) throw new Error("log sink is closed");

      const byteLength = Buffer.byteLength(output);
      if (this.bytesWritten > 0 && this.bytesWritten + byteLength > this.retention.maxFileBytes) {
        this.rotate(now);
      }
      if (this.fd === null) throw new Error("log sink is unavailable after rotation");

      // Harness-hosted plugins can exit without draining timers. A complete line is
      // therefore assembled first and issued as one synchronous write, preserving the tail.
      const written = fs.writeSync(this.fd, output, null, "utf8");
      if (written !== byteLength) throw new Error(`short log write (${written}/${byteLength})`);
      this.bytesWritten += written;
    } catch (error) {
      this.swallowedWrites += 1;
      if (!writeFailureReported) {
        writeFailureReported = true;
        reportStderr(`@cortexkit/log: log write failed; dropping lines: ${String(error)}`);
      }
    }
  }

  stats(): LoggerStats {
    return {
      swallowedWrites: this.swallowedWrites,
      fallbackActive: this.fallbackActive,
      path: this.path,
      ansiStripped: this.ansiStripped,
    };
  }

  async flush(): Promise<void> {
    // Synchronous writes leave no library-owned timer buffer for flush to drain.
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.fd !== null) {
      try {
        fs.closeSync(this.fd);
      } catch (error) {
        this.swallowedWrites += 1;
        if (!writeFailureReported) {
          writeFailureReported = true;
          reportStderr(`@cortexkit/log: log close failed: ${String(error)}`);
        }
      } finally {
        this.fd = null;
      }
    }
  }

  private openActive(): void {
    this.fd = fs.openSync(
      this.path,
      fs.constants.O_APPEND | fs.constants.O_CREAT | fs.constants.O_WRONLY,
      0o600,
    );
    if (process.platform !== "win32") fs.chmodSync(this.path, 0o600);
    this.bytesWritten = fs.fstatSync(this.fd).size;
  }

  private rotate(now: number): void {
    if (this.fd !== null) {
      fs.closeSync(this.fd);
      this.fd = null;
    }

    if (this.retention.keep === 0) {
      if (fs.existsSync(this.path)) fs.unlinkSync(this.path);
    } else {
      const oldest = `${this.path}.${this.retention.keep}`;
      if (fs.existsSync(oldest)) fs.unlinkSync(oldest);
      for (let generation = this.retention.keep - 1; generation >= 1; generation -= 1) {
        const source = `${this.path}.${generation}`;
        if (fs.existsSync(source)) fs.renameSync(source, `${this.path}.${generation + 1}`);
      }
      if (fs.existsSync(this.path)) fs.renameSync(this.path, `${this.path}.1`);
    }

    this.openActive();
    this.pruneGenerations(now);
  }

  private pruneGenerations(now: number): void {
    const cutoff = now - this.retention.maxAgeDays * DAY_MS;
    for (let generation = 1; generation <= this.retention.keep; generation += 1) {
      const generationPath = `${this.path}.${generation}`;
      try {
        if (fs.statSync(generationPath).mtimeMs < cutoff) fs.unlinkSync(generationPath);
      } catch (error) {
        const code = (error as NodeJS.ErrnoException).code;
        if (code !== "ENOENT") throw error;
      }
    }
  }
}

class LoggerImpl implements Logger {
  constructor(
    private readonly moduleId: string,
    private readonly declaredTags: ReadonlySet<string>,
    private readonly filter: ParsedFilter,
    private readonly sink: FileSink,
    private readonly session: Session | null,
  ) {}

  error(message: string, fields?: Record<string, FieldValue>, opts?: { tag?: string; session?: Session }): void {
    this.log("error", message, fields, opts);
  }

  warn(message: string, fields?: Record<string, FieldValue>, opts?: { tag?: string; session?: Session }): void {
    this.log("warn", message, fields, opts);
  }

  info(message: string, fields?: Record<string, FieldValue>, opts?: { tag?: string; session?: Session }): void {
    this.log("info", message, fields, opts);
  }

  debug(message: string, fields?: Record<string, FieldValue>, opts?: { tag?: string; session?: Session }): void {
    this.log("debug", message, fields, opts);
  }

  trace(message: string, fields?: Record<string, FieldValue>, opts?: { tag?: string; session?: Session }): void {
    this.log("trace", message, fields, opts);
  }

  withSession(issuer: string, id: string): Logger {
    validateToken("session issuer", issuer);
    validateToken("session id", id);
    return new LoggerImpl(
      this.moduleId,
      this.declaredTags,
      this.filter,
      this.sink,
      { issuer, id },
    );
  }

  enabled(level: Level, tag?: string): boolean {
    if (!LEVELS.includes(level)) throw new Error(`unknown log level: ${String(level)}`);
    this.assertDeclaredTag(tag);
    const configured = tag ? (this.filter.tags.get(tag) ?? this.filter.defaultLevel) : this.filter.defaultLevel;
    return permits(configured, level);
  }

  stats(): LoggerStats {
    return this.sink.stats();
  }

  flush(): Promise<void> {
    return this.sink.flush();
  }

  close(): Promise<void> {
    return this.sink.close();
  }

  private log(
    level: Level,
    message: string,
    fields?: Record<string, FieldValue>,
    opts?: { tag?: string; session?: Session },
  ): void {
    this.assertDeclaredTag(opts?.tag);
    if (!this.enabled(level, opts?.tag)) return;
    const now = this.sink.clock();
    const formatted = formatLineWithStats({
      at_ms: now,
      level,
      module: this.moduleId,
      session: opts?.session ?? this.session,
      tag: opts?.tag ?? null,
      message,
      fields: Object.entries(fields ?? {}),
    });
    this.sink.write(formatted.line, now, formatted.ansiStripped);
  }

  private assertDeclaredTag(tag: string | undefined): void {
    if (tag !== undefined && !this.declaredTags.has(tag)) {
      throw new Error(`undeclared log tag: ${tag}`);
    }
  }
}

export function createLogger(config: LogConfig): Logger {
  const declaredTags = new Set<string>();
  for (const tag of config.tags) {
    validateToken("declared tag", tag);
    if (declaredTags.has(tag)) throw new Error(`duplicate declared log tag: ${tag}`);
    declaredTags.add(tag);
  }

  const filter = parseFilter(config.spec ?? process.env.CK_LOG ?? "", declaredTags);
  const sink = new FileSink(config);
  return new LoggerImpl(config.moduleId, declaredTags, filter, sink, null);
}
