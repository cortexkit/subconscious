import * as fs from "node:fs";
import * as path from "node:path";

import { moduleDataDir } from "@cortexkit/store";

export type Level = "error" | "warn" | "info" | "debug" | "trace";

export interface Session {
  issuer: string;
  id: string;
}

export type FieldValue = string | number | boolean;

/**
 * Bound context in render order. The array form is canonical because the line
 * format pins the order fields were bound in; a plain object is accepted at
 * call sites and keeps its insertion order.
 */
export type BoundFields =
  | ReadonlyArray<readonly [string, FieldValue]>
  | Record<string, FieldValue>;

/**
 * One structured event. The field names are the ones the shared golden fixture
 * (`crates/subc-core/tests/fixtures/log_format_golden.json`) uses, so a case
 * from it can be rendered without any translation step.
 */
export interface LogEvent {
  at_ms: number;
  level: Level;
  /** Dotted logger name rooted at the module id: `engram`, `engram.gc.walk`. */
  logger: string;
  /** Context bound to a scope, rendered in a bracket before the message. */
  bound?: ReadonlyArray<readonly [string, FieldValue]>;
  message: string;
  fields: ReadonlyArray<readonly [string, FieldValue]>;
}

export interface ParsedLine {
  at_ms: number;
  timestamp: string;
  level: Level;
  /** The full dotted logger name. */
  logger: string;
  /** The logger's first segment, which is always the module id. */
  moduleId: string;
  /** Decoded bound fields in the order they were rendered. */
  bound: Array<[string, string]>;
  /** The `session=` bound value split at its issuer prefix, when present. */
  session: Session | null;
  message: string;
  fields: Array<[string, string]>;
}

export interface LogConfig {
  /** Stable fleet module id: the root of every logger name and of every file name. */
  moduleId: string;
  /**
   * Where the day segments live. Callers normally omit this so the path comes
   * from the same resolver every store uses; a log then cannot land beside the
   * wrong module's data.
   */
  logsDir?: string;
  /**
   * Fields bound for the whole process, rendered first and in this order. A
   * harness-hosted plugin binds `harness=<name>` here: every lane of a module
   * shares one file, and this is what tells them apart.
   */
  bound?: BoundFields;
  /** `CK_LOG` override; omitted reads the process environment. */
  spec?: string;
  /** Segments older than this many days are unlinked. Default 14. */
  maxAgeDays?: number;
  /** Size at which today's segment is reported as oversized. Default 256 MiB. */
  alarmSegmentMb?: number;
  /** Module redactor, applied after the fleet credential redactor. */
  redact?: (line: string) => string;
  /** Clock override, for deterministic callers and tests. */
  clock?: () => Date;
}

export interface LoggerStats {
  swallowedWrites: number;
  fallbackActive: boolean;
  /** Directory holding the day segments. */
  logsDir: string;
  /** The segment path for the current instant. */
  path: string;
  ansiStripped: number;
}

export interface Logger {
  error(message: string, fields?: Record<string, FieldValue>): void;
  warn(message: string, fields?: Record<string, FieldValue>): void;
  info(message: string, fields?: Record<string, FieldValue>): void;
  debug(message: string, fields?: Record<string, FieldValue>): void;
  trace(message: string, fields?: Record<string, FieldValue>): void;
  /** A logger named `<current>.<component>`, sharing this logger's file. */
  child(component: string): Logger;
  /** A logger carrying extra bound fields for a scope of work. */
  withBound(fields: BoundFields): Logger;
  /** `withBound` for the canonical session field. Empty halves bind nothing. */
  withSession(issuer: string, id: string): Logger;
  enabled(level: Level): boolean;
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
const SEGMENT_PATTERN = /^[a-z][a-z0-9-]*$/;
const DAY_MS = 24 * 60 * 60 * 1_000;
const MIB = 1024 * 1024;
const DEFAULT_MAX_AGE_DAYS = 14;
const DEFAULT_ALARM_SEGMENT_MB = 256;
const REDACTED = "[REDACTED]";

let malformedSpecReported = false;
let writeFailureReported = false;

/** A level threshold, where "off" emits nothing at all. */
type Threshold = Level | "off";

interface LevelFilter {
  /** Applied when no directive matches the logger. */
  root: Threshold;
  /** `[dotted prefix, threshold]`, shallowest first so the last hit is the most specific. */
  directives: Array<[string, Threshold]>;
}

interface FormatResult {
  line: string;
  ansiStripped: number;
}

function isSegment(value: string): boolean {
  return SEGMENT_PATTERN.test(value);
}

function assertSegment(label: string, value: string): void {
  if (!isSegment(value)) {
    throw new Error(`${label} must match [a-z][a-z0-9-]*, got ${JSON.stringify(value)}`);
  }
}

function assertFieldKey(label: string, key: string): void {
  if (key.length === 0 || /[\s="\]]/.test(key)) {
    throw new Error(`${label} must be a non-empty token without whitespace, '=', '"' or ']'`);
  }
}

/**
 * Removes the terminal escape sequences that are complete inside one field and
 * counts them. Only two shapes count as a sequence: CSI (`ESC [` or C1 `U+009B`,
 * parameter and intermediate bytes `0x20`-`0x3f`, then a final byte
 * `0x40`-`0x7e`) and OSC (`ESC ]` or C1 `U+009D`, ended by `BEL`, `ESC \` or C1
 * `U+009C`). An introducer whose sequence does not end inside the field is left
 * in place, so the escaping step writes it out as text instead of letting it
 * swallow whatever follows.
 */
function stripCompleteSequences(value: string): { value: string; count: number } {
  if (!/[\u001b\u009b\u009d]/.test(value)) return { value, count: 0 };

  let output = "";
  let count = 0;
  let index = 0;
  while (index < value.length) {
    const char = value[index]!;
    const next = value[index + 1];
    const isCsi = (char === "\u001b" && next === "[") || char === "\u009b";
    const isOsc = (char === "\u001b" && next === "]") || char === "\u009d";
    let end = -1;

    if (isCsi) {
      let cursor = index + (char === "\u001b" ? 2 : 1);
      while (cursor < value.length) {
        const code = value.charCodeAt(cursor);
        if (code >= 0x40 && code <= 0x7e) {
          end = cursor + 1;
          break;
        }
        if (code < 0x20 || code > 0x3f) break;
        cursor += 1;
      }
    } else if (isOsc) {
      let cursor = index + (char === "\u001b" ? 2 : 1);
      while (cursor < value.length) {
        const code = value.charCodeAt(cursor);
        if (code === 0x07 || code === 0x9c) {
          end = cursor + 1;
          break;
        }
        if (code === 0x1b && value[cursor + 1] === "\\") {
          end = cursor + 2;
          break;
        }
        cursor += 1;
      }
    }

    if (end < 0) {
      output += char;
      index += 1;
    } else {
      count += 1;
      index = end;
    }
  }
  return { value: output, count };
}

// Every control character other than `\r` and `\n` (which have their own short
// escapes): C0, DEL and the C1 range.
const CONTROL_PATTERN = /[\u0000-\u0009\u000b\u000c\u000e-\u001f\u007f-\u009f]/;
const CONTROL_PATTERN_GLOBAL = new RegExp(CONTROL_PATTERN.source, "g");

/** Writes line breaks as `\r`/`\n` and every other control character as `\uXXXX`. */
function escapeControls(value: string): string {
  return value
    .replaceAll("\r", "\\r")
    .replaceAll("\n", "\\n")
    .replace(
      CONTROL_PATTERN_GLOBAL,
      (char) => `\\u${char.charCodeAt(0).toString(16).padStart(4, "0")}`,
    );
}

/** Collects how many escape sequences the per-field strip removed for one line. */
interface StripCounter {
  count: number;
}

// Backslash is escaped first so text that merely looks like an escape (a
// literal `\n` or `\u0041`) survives as `\\n` / `\\u0041` and cannot be read
// back as one. Same order as the Rust twin.
function escapeMessage(value: string, counter: StripCounter): string {
  const stripped = stripCompleteSequences(value);
  counter.count += stripped.count;
  return escapeControls(stripped.value.replaceAll("\\", "\\\\"));
}

function formatValue(value: FieldValue, counter: StripCounter): string {
  const stripped = stripCompleteSequences(typeof value === "string" ? value : String(value));
  counter.count += stripped.count;
  const text = stripped.value;
  // A `]` is quoted along with the whitespace and quote characters so a bound
  // value can never be misread as the end of the bracket. A control character
  // forces quoting too, because only the quoted form can carry its escape.
  if (text !== "" && !/[ "\r\n\]]/.test(text) && !CONTROL_PATTERN.test(text)) {
    // An unquoted value is verbatim, backslashes included: only the quoted
    // form has an escape grammar, so only a quoted value is ever decoded.
    return text;
  }
  const escaped = escapeControls(text.replaceAll("\\", "\\\\").replaceAll('"', '\\"'));
  return `"${escaped}"`;
}

// Each field is stripped and escaped on its own before it is placed in the
// line. Stripping the assembled line instead lets a stray `ESC ]` in one value
// start an OSC that eats that value's closing quote and every later field.
function formatLineWithStats(event: LogEvent): FormatResult {
  if (!LEVELS.includes(event.level)) throw new Error(`unknown log level: ${String(event.level)}`);
  for (const segment of event.logger.split(".")) assertSegment("logger segment", segment);

  const counter: StripCounter = { count: 0 };
  let line = `${new Date(event.at_ms).toISOString()} ${event.level
    .toUpperCase()
    .padEnd(5, " ")} ${event.logger}:`;

  const bound = event.bound ?? [];
  // Nothing bound renders no bracket at all: "nothing is bound" and "an empty
  // context" are different facts, and only the first one is ever true.
  if (bound.length > 0) {
    const rendered = bound.map(([key, value]) => {
      assertFieldKey("bound field key", key);
      return `${key}=${formatValue(value, counter)}`;
    });
    line += ` [${rendered.join(" ")}]`;
  }
  if (event.message !== "") line += ` ${escapeMessage(event.message, counter)}`;
  for (const [key, value] of event.fields) {
    assertFieldKey("event field key", key);
    line += ` ${key}=${formatValue(value, counter)}`;
  }

  return { line, ansiStripped: counter.count };
}

/** Render one canonical fleet log line without a trailing newline. */
export function formatLine(event: LogEvent): string {
  return formatLineWithStats(event).line;
}

interface Token {
  text: string;
  start: number;
}

/** Splits on unquoted spaces, keeping each token's offset and its quoting intact. */
function tokenize(input: string): Token[] | null {
  const tokens: Token[] = [];
  let text = "";
  let start = 0;
  let quoted = false;
  let escaped = false;

  for (let index = 0; index < input.length; index += 1) {
    const char = input[index]!;
    if (escaped) {
      text += char;
      escaped = false;
      continue;
    }
    if (quoted && char === "\\") {
      text += char;
      escaped = true;
      continue;
    }
    if (char === '"') {
      quoted = !quoted;
      text += char;
      continue;
    }
    if (char === " " && !quoted) {
      if (text.length > 0) {
        tokens.push({ text, start });
        text = "";
      }
      start = index + 1;
      continue;
    }
    if (text.length === 0) start = index;
    text += char;
  }

  if (quoted || escaped) return null;
  if (text.length > 0) tokens.push({ text, start });
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
    else if (escaped === "u" && /^[0-9a-fA-F]{4}$/.test(value.slice(index + 1, index + 5))) {
      // `\uXXXX` is how the renderer writes a control character.
      decoded += String.fromCharCode(Number.parseInt(value.slice(index + 1, index + 5), 16));
      index += 4;
    } else decoded += escaped;
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

// The bracket closes at the first `]` outside a quoted value. A bound value
// containing `]` was quoted by the renderer for exactly this reason, so a
// plain search for `]` would split a path like `[root="a]b"]` in the middle.
function findBracketClose(input: string): number {
  let quoted = false;
  let escaped = false;
  for (let index = 0; index < input.length; index += 1) {
    const char = input[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (quoted && char === "\\") {
      escaped = true;
      continue;
    }
    if (char === '"') quoted = !quoted;
    else if (char === "]" && !quoted) return index;
  }
  return -1;
}

function splitSession(value: string): Session | null {
  const separator = value.lastIndexOf(":");
  if (separator <= 0 || separator === value.length - 1) return null;
  return { issuer: value.slice(0, separator), id: value.slice(separator + 1) };
}

const LEVEL_COLUMNS: ReadonlyArray<readonly [string, Level]> = [
  ["TRACE ", "trace"],
  ["DEBUG ", "debug"],
  ["INFO  ", "info"],
  ["WARN  ", "warn"],
  ["ERROR ", "error"],
];

/** Parse a canonical fleet line, returning a stable reason for malformed input. */
export function parseLine(line: string): ParsedLine | { reject: string } {
  if (/[\u001b\u009b]/.test(line)) return { reject: "ansi_forbidden" };
  if (/[\r\n]/.test(line)) return { reject: "line_break" };

  const timestampEnd = line.indexOf(" ");
  if (timestampEnd < 0) return { reject: "timestamp_missing" };
  const timestamp = line.slice(0, timestampEnd);
  if (!timestamp.endsWith("Z")) return { reject: "timestamp_not_utc_z" };
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(timestamp)) {
    return { reject: "timestamp_precision" };
  }
  const atMs = Date.parse(timestamp);
  if (!Number.isFinite(atMs) || new Date(atMs).toISOString() !== timestamp) {
    return { reject: "timestamp_invalid" };
  }

  const afterTimestamp = line.slice(timestampEnd + 1);
  const column = LEVEL_COLUMNS.find(([prefix]) => afterTimestamp.startsWith(prefix));
  if (!column) {
    const named = LEVELS.some((level) => afterTimestamp.startsWith(level.toUpperCase()));
    return { reject: named ? "level_column_width" : "level_invalid" };
  }
  const afterLevel = afterTimestamp.slice(column[0].length);

  // The logger runs to the first space and MUST end in a colon. A line in the
  // older format (`fusiform poll changed`) has no colon and fails here by
  // design: the colon is what separates the logger from a one-word message.
  const loggerEnd = afterLevel.indexOf(" ");
  const loggerToken = loggerEnd < 0 ? afterLevel : afterLevel.slice(0, loggerEnd);
  let body = loggerEnd < 0 ? "" : afterLevel.slice(loggerEnd + 1);
  if (!loggerToken.endsWith(":")) return { reject: "logger_not_terminated" };
  const logger = loggerToken.slice(0, -1);
  if (logger.length === 0) return { reject: "logger_missing" };
  const segments = logger.split(".");
  if (!segments.every(isSegment)) return { reject: "logger_segment_grammar" };

  const bound: Array<[string, string]> = [];
  let hadBracket = false;
  if (body.startsWith("[")) {
    const rest = body.slice(1);
    const close = findBracketClose(rest);
    if (close < 0) return { reject: "bound_unterminated" };
    const inside = rest.slice(0, close);
    if (inside.length === 0) return { reject: "empty_bound_bracket" };
    const boundTokens = tokenize(inside);
    if (!boundTokens) return { reject: "bound_field_grammar" };
    for (const token of boundTokens) {
      const pair = decodeFieldToken(token.text);
      if (!pair) return { reject: "bound_field_grammar" };
      bound.push(pair);
    }
    const sessionPair = bound.find(([key]) => key === "session");
    // `session=global` and other issuer-less placeholders fail on their
    // missing `issuer:` half, so no particular sentinel has to be known here.
    if (sessionPair && !splitSession(sessionPair[1])) {
      return { reject: "session_missing_issuer" };
    }
    hadBracket = true;
    const afterBracket = rest.slice(close + 1);
    body = afterBracket.startsWith(" ") ? afterBracket.slice(1) : afterBracket;
  }

  // A bracket AFTER the message is not context: context precedes the message so
  // its column stays stable. Rejecting it keeps writers from putting it wherever
  // and losing that property; a trailing bracket is an event field value.
  if (!hadBracket && body.includes(" [") && body.endsWith("]")) {
    return { reject: "bound_after_message" };
  }

  const tokens = tokenize(body);
  if (!tokens) return { reject: "field_quoting_invalid" };

  // Nothing marks where the message ends and the fields begin, so the split is
  // the earliest point from which every remaining token parses as `key=value`.
  let fieldsStart = tokens.length;
  for (let index = 0; index < tokens.length; index += 1) {
    if (tokens.slice(index).every((token) => decodeFieldToken(token.text) !== null)) {
      fieldsStart = index;
      break;
    }
  }

  const session = bound.find(([key]) => key === "session");
  const fields = tokens.slice(fieldsStart).map((token) => decodeFieldToken(token.text)!);
  const messageEnd = fieldsStart < tokens.length ? tokens[fieldsStart]!.start : body.length;
  const message = decodeEscapedText(body.slice(0, messageEnd).trimEnd());
  if (message === null) return { reject: "message_escape_invalid" };

  return {
    at_ms: atMs,
    timestamp,
    level: column[1],
    logger,
    moduleId: segments[0]!,
    bound,
    session: session === undefined ? null : splitSession(session[1]),
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

function parseThreshold(text: string): Threshold | null {
  const lowered = text.toLowerCase();
  if (lowered === "off") return "off";
  return LEVELS.includes(lowered as Level) ? (lowered as Level) : null;
}

/**
 * Parses `CK_LOG` in `RUST_LOG`'s grammar: comma-separated directives, each
 * `<logger>=<level>` or a bare `<level>` that sets the root default.
 */
function parseSpec(spec: string): LevelFilter | { error: string } {
  let root: Threshold = "info";
  const directives: Array<[string, Threshold]> = [];

  for (const raw of spec.split(",")) {
    const directive = raw.trim();
    if (directive === "") continue;
    const equals = directive.indexOf("=");
    if (equals < 0) {
      const level = parseThreshold(directive);
      if (level === null) return { error: `unknown level: ${JSON.stringify(directive)}` };
      root = level;
      continue;
    }
    const logger = directive.slice(0, equals).trim();
    const levelText = directive.slice(equals + 1).trim();
    if (logger === "" || !logger.split(".").every(isSegment)) {
      return { error: `logger name is not <segment>(.<segment>)*: ${JSON.stringify(directive)}` };
    }
    if (levelText === "") {
      return { error: `directive has no level after '=': ${JSON.stringify(directive)}` };
    }
    const level = parseThreshold(levelText);
    if (level === null) return { error: `unknown level: ${JSON.stringify(directive)}` };
    directives.push([logger, level]);
  }

  // Shallowest first, so a single scan that keeps the last hit is "most
  // specific directive wins". Array sort is stable, so directives of equal
  // depth keep the order they were written in.
  directives.sort(([left], [right]) => left.split(".").length - right.split(".").length);
  return { root, directives };
}

// A directive names a prefix on DOTTED SEGMENTS, not on characters: `aft`
// covers `aft` and `aft.index`, and does not cover `aftershock`.
function coversLogger(prefix: string, logger: string): boolean {
  if (!logger.startsWith(prefix)) return false;
  const rest = logger.slice(prefix.length);
  return rest === "" || rest.startsWith(".");
}

function filterAllows(filter: LevelFilter, logger: string, level: Level): boolean {
  let threshold = filter.root;
  for (const [prefix, directive] of filter.directives) {
    if (coversLogger(prefix, logger)) threshold = directive;
  }
  return threshold !== "off" && LEVEL_RANK[level] <= LEVEL_RANK[threshold];
}

function resolveFilter(specOverride: string | undefined): LevelFilter {
  const raw = (specOverride ?? process.env.CK_LOG ?? "").trim();
  if (raw === "") return { root: "info", directives: [] };
  const parsed = parseSpec(raw);
  if ("error" in parsed) {
    if (!malformedSpecReported) {
      malformedSpecReported = true;
      reportStderr(
        `@cortexkit/log: invalid CK_LOG value ${JSON.stringify(raw)}; using info: ${parsed.error}`,
      );
    }
    return { root: "info", directives: [] };
  }
  return parsed;
}

/** Redact known credential shapes before optional caller redaction. */
/**
 * The fleet credential redactor, applied to every complete line before any
 * caller redactor. Exported so a reader or a caller composing its own sanitizer
 * can run the same rules; the shared fixture's `redaction` cases pin it.
 */
export function fleetRedact(line: string): string {
  return defaultRedactor(line);
}

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
    .replace(/\bgh[pousr]_[A-Za-z0-9_-]+\b/g, REDACTED)
    .replace(/\bgithub_pat_[A-Za-z0-9_]+\b/g, REDACTED)
    // URL userinfo (`user:pass@` or `token@`). The run cannot cross whitespace,
    // a path, query or fragment delimiter, or a quote, so an `@` later in the
    // URL or in an ordinary email address is never taken for userinfo.
    .replace(/\b([A-Za-z][A-Za-z0-9+.-]*:\/\/)[^\s/?#"@]+@/g, `$1${REDACTED}@`)
    // Credential query parameters, value only. This is its own pattern because
    // a generic `key=value` rule reads `https:` as the key and misses
    // `?access_token=`. The value consumes backslash escape pairs so an escaped
    // quote inside a quoted field value is redacted rather than split, which
    // would unbalance the field's quoting. Parentheses and brackets end the
    // value so URL wrappers do not swallow the diagnostic text after them.
    .replace(
      /([?&](?:access_token|token|api_key|apikey|password|secret|client_secret)=)(?:\\.|[^&#\s"\\)\]])+/gi,
      `$1${REDACTED}`,
    );
}

function utcDay(at: Date): string {
  return at.toISOString().slice(0, 10);
}

/** The file name for a module's segment on the UTC day containing `at`. */
export function segmentName(moduleId: string, at: Date): string {
  return `${moduleId}.${utcDay(at)}.log`;
}

/**
 * The UTC day in `<module_id>.<YYYY-MM-DD>.log`, or null for any name that is
 * not exactly that shape — another module's segment, a stderr capture, a
 * pid-suffixed file from the previous format, or a date that is not a real
 * calendar day. Those are left alone rather than guessed at.
 */
export function segmentDay(moduleId: string, fileName: string): string | null {
  const prefix = `${moduleId}.`;
  const suffix = ".log";
  if (!fileName.startsWith(prefix) || !fileName.endsWith(suffix)) return null;
  const day = fileName.slice(prefix.length, fileName.length - suffix.length);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(day)) return null;
  const parsed = Date.parse(`${day}T00:00:00.000Z`);
  if (!Number.isFinite(parsed)) return null;
  // Rejects a date that parses loosely but is not the day it spells, such as
  // a month of 13 or a 30th of February.
  return utcDay(new Date(parsed)) === day ? day : null;
}

function dayOffset(day: string, days: number): string {
  return utcDay(new Date(Date.parse(`${day}T00:00:00.000Z`) - days * DAY_MS));
}

/**
 * The segments to unlink, decided by the date in the file name alone: no stat,
 * no mtime. The boundary day (`today - maxAgeDays`) is KEPT, so `maxAgeDays: 0`
 * keeps today only and the active segment is never a candidate.
 */
export function pruneCandidates(
  moduleId: string,
  today: string,
  maxAgeDays: number,
  present: readonly string[],
): string[] {
  const boundary = dayOffset(today, maxAgeDays);
  return present.filter((name) => {
    const day = segmentDay(moduleId, name);
    return day !== null && day < boundary;
  });
}

interface SinkOptions {
  moduleId: string;
  logsDir: string;
  maxAgeDays: number;
  alarmBytes: number;
  clock: () => Date;
  redact?: (line: string) => string;
}

/**
 * Appends to `<logsDir>/<moduleId>.<UTC day>.log`, reopening when the day
 * rolls. The name is never renamed, which is what lets every process of a
 * module — the module itself and each harness-hosted plugin — share one file:
 * each writer derives the same name from the clock and opens it `O_APPEND`, so
 * the kernel lands every line whole with no lock and no coordinator.
 */
class SegmentSink {
  readonly moduleId: string;
  readonly logsDir: string;
  readonly maxAgeDays: number;
  readonly alarmBytes: number;
  readonly clock: () => Date;
  readonly redact?: (line: string) => string;
  swallowedWrites = 0;
  fallbackActive = false;
  ansiStripped = 0;
  private fd: number | null = null;
  private openDay: string | null = null;
  private alarmed = false;
  private closed = false;

  constructor(options: SinkOptions) {
    this.moduleId = options.moduleId;
    this.logsDir = options.logsDir;
    this.maxAgeDays = options.maxAgeDays;
    this.alarmBytes = options.alarmBytes;
    this.clock = options.clock;
    this.redact = options.redact;

    try {
      fs.mkdirSync(this.logsDir, { recursive: true, mode: 0o700 });
      if (process.platform !== "win32") fs.chmodSync(this.logsDir, 0o700);
      this.rollTo(utcDay(this.clock()));
    } catch (error) {
      this.fallbackActive = true;
      reportStderr(
        `@cortexkit/log: cannot open ${this.pathNow()}; falling back to stderr: ${String(error)}`,
      );
    }
  }

  pathNow(): string {
    return path.join(this.logsDir, segmentName(this.moduleId, this.clock()));
  }

  write(line: string, at: Date, ansiCount: number): void {
    this.ansiStripped += ansiCount;
    try {
      let redacted = defaultRedactor(line);
      if (this.redact) redacted = this.redact(redacted);
      // Redactors are extension points, so the last guard before the write
      // keeps the one-line, no-raw-control contract even when one introduces a
      // break or an escape sequence. It escapes and never strips: stripping
      // across the whole line is what let one bad value eat the fields after it.
      const guarded = escapeControls(redacted);
      const output = `${guarded}\n`;

      if (this.fallbackActive) {
        process.stderr.write(output);
        return;
      }
      if (this.closed) throw new Error("log sink is closed");

      const today = utcDay(at);
      if (this.openDay !== today) this.rollTo(today);
      if (this.fd === null) throw new Error("log segment is not open");

      // Harness-hosted plugins can exit without draining timers, so a complete
      // line is assembled first and issued as one synchronous write.
      const byteLength = Buffer.byteLength(output);
      const written = fs.writeSync(this.fd, output, null, "utf8");
      if (written !== byteLength) throw new Error(`short log write (${written}/${byteLength})`);
      this.reportOversize();
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
      logsDir: this.logsDir,
      path: this.pathNow(),
      ansiStripped: this.ansiStripped,
    };
  }

  async flush(): Promise<void> {
    // Synchronous writes leave no library-owned buffer for flush to drain.
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.fd === null) return;
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

  // Opening a segment also prunes: the writer is the only party that runs
  // whenever the module runs, so it is the only one that can bound the set
  // without a daemon.
  private rollTo(day: string): void {
    if (this.fd !== null) {
      fs.closeSync(this.fd);
      this.fd = null;
    }
    const pruned = this.prune(day);
    const segmentPath = path.join(this.logsDir, `${this.moduleId}.${day}.log`);
    this.fd = fs.openSync(segmentPath, "a", 0o600);
    if (process.platform !== "win32") fs.chmodSync(segmentPath, 0o600);
    this.openDay = day;
    this.alarmed = false;
    // A feature that fires later has to say that it fired: without this line,
    // "retention never ran" and "ran and found nothing" look the same from
    // outside the process.
    if (pruned.removed > 0) {
      reportStderr(
        `@cortexkit/log: ${this.moduleId}.retention pruned=${pruned.removed} kept=${pruned.kept}`,
      );
    }
  }

  private prune(today: string): { removed: number; kept: number } {
    const segments = fs
      .readdirSync(this.logsDir)
      .filter((name) => segmentDay(this.moduleId, name) !== null);
    const doomed = pruneCandidates(this.moduleId, today, this.maxAgeDays, segments);
    for (const name of doomed) {
      try {
        fs.unlinkSync(path.join(this.logsDir, name));
      } catch (error) {
        // Another writer for this module pruned it first. That is the design
        // working, not a failure.
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
    }
    return { removed: doomed.length, kept: segments.length - doomed.length };
  }

  // Reported once per segment, because a module writing this much in a day has
  // a defect worth surfacing and a runaway writer must not get a second flood
  // from its own alarm. The segment is never truncated: that would hide it.
  private reportOversize(): void {
    if (this.alarmed || this.fd === null) return;
    const size = fs.fstatSync(this.fd).size;
    if (size <= this.alarmBytes) return;
    this.alarmed = true;
    reportStderr(
      `@cortexkit/log: segment oversized, NOT truncated: path=${path.join(
        this.logsDir,
        `${this.moduleId}.${this.openDay}.log`,
      )} bytes=${size}`,
    );
  }
}

function boundPairs(fields: BoundFields): ReadonlyArray<readonly [string, FieldValue]> {
  return Array.isArray(fields)
    ? (fields as ReadonlyArray<readonly [string, FieldValue]>)
    : Object.entries(fields as Record<string, FieldValue>);
}

/**
 * Merges bound fields left to right: an existing key is overwritten IN PLACE so
 * an inner scope refines an outer one without moving the bracket's columns, and
 * a new key is appended. An empty value binds nothing — a line carries no field
 * rather than a `key=` with no value.
 */
function mergeBound(
  base: ReadonlyArray<readonly [string, string]>,
  additions: BoundFields,
): Array<[string, string]> {
  const merged: Array<[string, string]> = base.map(([key, value]) => [key, value]);
  for (const [key, rawValue] of boundPairs(additions)) {
    assertFieldKey("bound field key", key);
    const value = typeof rawValue === "string" ? rawValue : String(rawValue);
    if (value === "") continue;
    const existing = merged.findIndex(([candidate]) => candidate === key);
    if (existing >= 0) merged[existing] = [key, value];
    else merged.push([key, value]);
  }
  return merged;
}

class LoggerImpl implements Logger {
  constructor(
    private readonly logger: string,
    private readonly bound: ReadonlyArray<readonly [string, string]>,
    private readonly filter: LevelFilter,
    private readonly sink: SegmentSink,
  ) {}

  error(message: string, fields?: Record<string, FieldValue>): void {
    this.log("error", message, fields);
  }

  warn(message: string, fields?: Record<string, FieldValue>): void {
    this.log("warn", message, fields);
  }

  info(message: string, fields?: Record<string, FieldValue>): void {
    this.log("info", message, fields);
  }

  debug(message: string, fields?: Record<string, FieldValue>): void {
    this.log("debug", message, fields);
  }

  trace(message: string, fields?: Record<string, FieldValue>): void {
    this.log("trace", message, fields);
  }

  child(component: string): Logger {
    assertSegment("logger component", component);
    return new LoggerImpl(`${this.logger}.${component}`, this.bound, this.filter, this.sink);
  }

  withBound(fields: BoundFields): Logger {
    return new LoggerImpl(this.logger, mergeBound(this.bound, fields), this.filter, this.sink);
  }

  withSession(issuer: string, id: string): Logger {
    // An empty half means "no session": the line carries no field rather than
    // a placeholder, because a placeholder is greppable against nothing.
    if (issuer === "" || id === "") return this;
    return this.withBound([["session", `${issuer}:${id}`]]);
  }

  enabled(level: Level): boolean {
    if (!LEVELS.includes(level)) throw new Error(`unknown log level: ${String(level)}`);
    return filterAllows(this.filter, this.logger, level);
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

  private log(level: Level, message: string, fields?: Record<string, FieldValue>): void {
    if (!this.enabled(level)) return;
    const at = this.sink.clock();
    const formatted = formatLineWithStats({
      at_ms: at.getTime(),
      level,
      logger: this.logger,
      bound: this.bound,
      message,
      fields: Object.entries(fields ?? {}),
    });
    this.sink.write(formatted.line, at, formatted.ansiStripped);
  }
}

function positiveNumber(label: string, value: number): number {
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${label} must be a positive number`);
  }
  return value;
}

function wholeDays(label: string, value: number): number {
  if (!Number.isInteger(value) || value < 0) {
    throw new Error(`${label} must be a non-negative whole number of days`);
  }
  return value;
}

// An unparseable environment value is ignored rather than fatal: a logger that
// refuses to start over a stray knob is worse than one on the defaults.
function envNumber(name: string): number | undefined {
  const raw = process.env[name]?.trim();
  if (raw === undefined || raw === "") return undefined;
  const value = Number(raw);
  return Number.isFinite(value) ? value : undefined;
}

/**
 * The config for a plugin running inside a harness process. `harness=` is
 * bound on every line because all of a module's lanes share one file, and it
 * is the only thing that separates them.
 */
export function forPlugin(moduleId: string, harness: string): LogConfig {
  return { moduleId, bound: [["harness", harness]] };
}

export function createLogger(config: LogConfig): Logger {
  // The module id is the root of the logger hierarchy that CK_LOG filters on,
  // so it has to satisfy the same segment grammar as every component.
  assertSegment("module id", config.moduleId);
  const logsDir = config.logsDir ?? path.join(moduleDataDir(config.moduleId), "logs");
  const maxAgeDays = wholeDays(
    "maxAgeDays",
    config.maxAgeDays ?? envNumber("CK_LOG_MAX_AGE_DAYS") ?? DEFAULT_MAX_AGE_DAYS,
  );
  const alarmSegmentMb = positiveNumber(
    "alarmSegmentMb",
    config.alarmSegmentMb ?? envNumber("CK_LOG_ALARM_SEGMENT_MB") ?? DEFAULT_ALARM_SEGMENT_MB,
  );

  const sink = new SegmentSink({
    moduleId: config.moduleId,
    logsDir,
    maxAgeDays,
    alarmBytes: Math.floor(alarmSegmentMb * MIB),
    clock: config.clock ?? (() => new Date()),
    redact: config.redact,
  });
  return new LoggerImpl(
    config.moduleId,
    mergeBound([], config.bound ?? []),
    resolveFilter(config.spec),
    sink,
  );
}
