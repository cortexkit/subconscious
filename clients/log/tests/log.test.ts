import { afterEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  createLogger,
  formatLine,
  parseLine,
  type LogConfig,
  type LogEvent,
} from "../src/index.js";

interface GoldenFixture {
  cases: Array<{ name: string; event: unknown; line: string }>;
  parse_rejects: Array<{ name: string; line: string; reason: string }>;
  level_filter: {
    cases: Array<{
      spec: string;
      level: LogEvent["level"];
      tag: string | null;
      emit: boolean;
    }>;
  };
}

const fixturePath = new URL(
  "../../../crates/subc-core/tests/fixtures/log_format_golden.json",
  import.meta.url,
);
const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8")) as GoldenFixture;
const tempDirectories: string[] = [];

function temporaryDirectory(): string {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "cortexkit-log-"));
  tempDirectories.push(directory);
  return directory;
}

function customConfig(
  file: string,
  overrides: Partial<Omit<LogConfig, "moduleId" | "lane" | "tags">> & {
    tags?: readonly string[];
  } = {},
): LogConfig {
  return {
    moduleId: "test-module",
    lane: { kind: "custom", path: file },
    tags: overrides.tags ?? [],
    clock: overrides.clock ?? (() => 1_788_604_863_123),
    spec: overrides.spec,
    retention: overrides.retention,
    redactor: overrides.redactor,
  };
}

function captureStderr<T>(run: () => T): { result: T; output: string } {
  let output = "";
  const stderr = process.stderr;
  const original = stderr.write;
  stderr.write = ((chunk: unknown) => {
    output += typeof chunk === "string" ? chunk : Buffer.from(chunk as ArrayBuffer).toString();
    return true;
  }) as typeof stderr.write;
  try {
    return { result: run(), output };
  } finally {
    stderr.write = original;
  }
}

afterEach(() => {
  for (const directory of tempDirectories.splice(0)) {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

describe("golden line format", () => {
  for (const golden of fixture.cases) {
    test(golden.name, () => {
      expect(formatLine(golden.event as LogEvent)).toBe(golden.line);
    });
  }

  for (const rejected of fixture.parse_rejects) {
    test(`rejects ${rejected.name}`, () => {
      expect(parseLine(rejected.line)).toEqual({ reject: rejected.reason });
    });
  }

  test("parses a formatted line into structured fields", () => {
    const golden = fixture.cases.find((entry) => entry.name === "session-and-tag");
    if (!golden) throw new Error("golden session-and-tag case is missing");
    const parsed = parseLine(golden.line);
    expect(parsed).not.toHaveProperty("reject");
    if ("reject" in parsed) throw new Error(parsed.reject);
    expect(parsed.level).toBe("warn");
    expect(parsed.module).toBe("magic-context");
    expect(parsed.session).toEqual({ issuer: "opencode", id: "ses_00fc88222ffe" });
    expect(parsed.tag).toBe("perf");
    expect(parsed.message).toBe("transform stage folded");
    expect(parsed.fields).toEqual([
      ["ms", "412"],
      ["retry", "2"],
    ]);
  });
});

describe("CK_LOG filtering", () => {
  for (const [index, filterCase] of fixture.level_filter.cases.entries()) {
    test(`${JSON.stringify(filterCase.spec)} ${filterCase.tag ?? "default"} ${filterCase.level}`, async () => {
      const file = path.join(temporaryDirectory(), `${index}.log`);
      const captured = captureStderr(() =>
        createLogger(customConfig(file, { spec: filterCase.spec, tags: ["perf", "wire"] })),
      );
      const logger = captured.result;
      expect(logger.enabled(filterCase.level, filterCase.tag ?? undefined)).toBe(filterCase.emit);
      if (filterCase.spec === "garbage=") {
        expect(captured.output.match(/malformed CK_LOG/g)).toHaveLength(1);
        const second = captureStderr(() =>
          createLogger(
            customConfig(path.join(temporaryDirectory(), "second-malformed.log"), {
              spec: filterCase.spec,
              tags: ["perf", "wire"],
            }),
          ),
        );
        expect(second.output).not.toContain("malformed CK_LOG");
        await second.result.close();
      }
      await logger.close();
    });
  }
});

test("withSession carries a session without changing its parent", async () => {
  const file = path.join(temporaryDirectory(), "sessions.log");
  const parent = createLogger(customConfig(file));
  const child = parent.withSession("opencode", "ses_child");

  child.info("from child");
  parent.info("from parent");
  await parent.close();

  const [childLine, parentLine] = fs.readFileSync(file, "utf8").trimEnd().split("\n");
  expect(childLine).toContain(" session=opencode:ses_child from child");
  expect(parentLine).not.toContain(" session=");
  expect(parentLine).toEndWith(" from parent");
});

test("undeclared tags throw while declared tags are emitted", async () => {
  const file = path.join(temporaryDirectory(), "tags.log");
  const logger = createLogger(customConfig(file, { tags: ["perf"] }));

  expect(() => logger.info("bad", undefined, { tag: "wire" })).toThrow("undeclared log tag: wire");
  expect(() => logger.info("good", undefined, { tag: "perf" })).not.toThrow();
  await logger.close();

  expect(fs.readFileSync(file, "utf8")).toContain(" tag=perf good");
});

test("module and plugin lanes use separate module-owned files", async () => {
  const dataHome = temporaryDirectory();
  const previous = process.env.XDG_DATA_HOME;
  process.env.XDG_DATA_HOME = dataHome;
  try {
    const moduleLogger = createLogger({
      moduleId: "magic-context",
      lane: { kind: "module" },
      tags: [],
      clock: () => 1_788_604_863_123,
    });
    const pluginLogger = createLogger({
      moduleId: "magic-context",
      lane: { kind: "plugin", harness: "opencode" },
      tags: [],
      clock: () => 1_788_604_863_123,
    });

    moduleLogger.info("module only");
    pluginLogger.info("plugin only");
    await Promise.all([moduleLogger.close(), pluginLogger.close()]);

    const logs = path.join(dataHome, "cortexkit", "magic-context", "logs");
    const modulePath = path.join(logs, "magic-context.log");
    const pluginPath = path.join(logs, "magic-context.opencode.log");
    expect(fs.readFileSync(modulePath, "utf8")).toContain("module only");
    expect(fs.readFileSync(modulePath, "utf8")).not.toContain("plugin only");
    expect(fs.readFileSync(pluginPath, "utf8")).toContain("plugin only");
    expect(fs.readFileSync(pluginPath, "utf8")).not.toContain("module only");
    if (process.platform !== "win32") {
      expect(fs.statSync(logs).mode & 0o777).toBe(0o700);
      expect(fs.statSync(modulePath).mode & 0o777).toBe(0o600);
      expect(fs.statSync(pluginPath).mode & 0o777).toBe(0o600);
    }
  } finally {
    if (previous === undefined) delete process.env.XDG_DATA_HOME;
    else process.env.XDG_DATA_HOME = previous;
  }
});

test("rotation keeps two generations", async () => {
  const file = path.join(temporaryDirectory(), "rotate.log");
  const retention = {
    maxFileMb: 32,
    keep: 2,
    maxAgeDays: 14,
    maxFileBytes: 70,
  } as LogConfig["retention"] & { maxFileBytes: number };
  const logger = createLogger(customConfig(file, { retention }));

  logger.info("first generation");
  logger.info("second generation");
  logger.info("active generation");
  await logger.close();

  expect(fs.readFileSync(file, "utf8")).toContain("active generation");
  expect(fs.readFileSync(`${file}.1`, "utf8")).toContain("second generation");
  expect(fs.readFileSync(`${file}.2`, "utf8")).toContain("first generation");
  expect(fs.existsSync(`${file}.3`)).toBe(false);
});

test("old generations are pruned at creation and rotation", async () => {
  const directory = temporaryDirectory();
  const file = path.join(directory, "age.log");
  const now = 1_788_604_863_123;
  const old = new Date(now - 15 * 24 * 60 * 60 * 1_000);
  fs.writeFileSync(`${file}.1`, "old at creation\n");
  fs.writeFileSync(`${file}.2`, "fresh\n");
  fs.utimesSync(`${file}.1`, old, old);
  fs.utimesSync(`${file}.2`, new Date(now), new Date(now));

  const retention = {
    maxFileMb: 32,
    keep: 2,
    maxAgeDays: 14,
    maxFileBytes: 70,
  } as LogConfig["retention"] & { maxFileBytes: number };
  const logger = createLogger(customConfig(file, { retention, clock: () => now }));
  expect(fs.existsSync(`${file}.1`)).toBe(false);
  expect(fs.existsSync(`${file}.2`)).toBe(true);

  logger.info("first active line");
  fs.writeFileSync(`${file}.1`, "old at rotation\n");
  fs.utimesSync(`${file}.1`, old, old);
  logger.info("rotate now");
  await logger.close();

  expect(fs.readFileSync(`${file}.1`, "utf8")).toContain("first active line");
  expect(fs.existsSync(`${file}.2`)).toBe(false);
});

test("an open failure announces stderr fallback on its first line", async () => {
  const directory = temporaryDirectory();
  const blocker = path.join(directory, "not-a-directory");
  fs.writeFileSync(blocker, "blocker");
  const target = path.join(blocker, "fallback.log");

  const captured = captureStderr(() => {
    const logger = createLogger(customConfig(target));
    logger.info("still visible");
    return logger;
  });
  await captured.result.close();

  const lines = captured.output.trimEnd().split("\n");
  expect(lines[0]).toContain("falling back to stderr");
  expect(lines[1]).toContain("still visible");
  expect(captured.result.stats()).toMatchObject({
    fallbackActive: true,
    swallowedWrites: 0,
    path: target,
  });
});

test("write failures are swallowed and reported only once", async () => {
  const directory = temporaryDirectory();
  const file = path.join(directory, "failure.log");
  const retention = {
    maxFileMb: 32,
    keep: 2,
    maxAgeDays: 14,
    maxFileBytes: 1,
  } as LogConfig["retention"] & { maxFileBytes: number };
  const logger = createLogger(customConfig(file, { retention }));
  logger.info("opens successfully");

  fs.unlinkSync(file);
  fs.rmdirSync(directory);
  const captured = captureStderr(() => {
    expect(() => logger.info("first failed write")).not.toThrow();
    expect(() => logger.info("second failed write")).not.toThrow();
  });
  await logger.close();

  expect(logger.stats().swallowedWrites).toBe(2);
  expect(captured.output.match(/log write failed/g)).toHaveLength(1);
});

describe("fleet redaction", () => {
  const shapes = [
    ["bearer", "Bearer abc.DEF_123", "abc.DEF_123"],
    ["jwt", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature", "eyJhbGciOiJIUzI1NiJ9"],
    ["ckh", "ckh_supersecret", "ckh_supersecret"],
    ["openai", "sk-supersecret", "sk-supersecret"],
    ["github personal", "ghp_supersecret", "ghp_supersecret"],
    ["github oauth", "gho_supersecret", "gho_supersecret"],
    ["authorization", "Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"],
  ] as const;

  for (const [name, value, secret] of shapes) {
    test(name, async () => {
      const file = path.join(temporaryDirectory(), `${name.replaceAll(" ", "-")}.log`);
      const logger = createLogger(customConfig(file));
      logger.info(`credential ${value}`);
      await logger.close();
      const line = fs.readFileSync(file, "utf8");
      expect(line).toContain("[REDACTED]");
      expect(line).not.toContain(secret);
    });
  }

  test("ordinary text passes through and caller redaction runs second", async () => {
    const file = path.join(temporaryDirectory(), "custom-redaction.log");
    const logger = createLogger(
      customConfig(file, { redactor: (line) => line.replace("customer-name", "[CUSTOM]") }),
    );
    logger.info("ordinary text customer-name");
    await logger.close();
    const line = fs.readFileSync(file, "utf8");
    expect(line).toContain("ordinary text [CUSTOM]");
    expect(line).not.toContain("customer-name");
  });
});

test("ANSI sequences are removed and counted", async () => {
  const file = path.join(temporaryDirectory(), "ansi.log");
  const logger = createLogger(customConfig(file));
  logger.info("color \x1b[31mred\x1b[0m");
  await logger.close();

  expect(fs.readFileSync(file, "utf8")).not.toContain("\x1b[");
  expect(logger.stats().ansiStripped).toBe(2);
});
