import { afterEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  createLogger,
  fleetRedact,
  forPlugin,
  formatLine,
  parseLine,
  pruneCandidates,
  segmentDay,
  segmentName,
  type Level,
  type LogConfig,
  type LogEvent,
  type Logger,
} from "../src/index.js";

interface GoldenFixture {
  schema: number;
  cases: Array<{ name: string; event: LogEvent; line: string }>;
  redaction: { cases: Array<{ name: string; input: string; output: string }> };
  parse_rejects: Array<{ name: string; line: string; reason: string }>;
  level_filter: {
    cases: Array<{ spec: string; level: Level; logger: string; emit: boolean }>;
  };
  segment_name: {
    cases: Array<{ module: string; at_ms: number; name: string }>;
  };
  retention_prune: {
    cases: Array<{
      name: string;
      module: string;
      today: string;
      max_age_days: number;
      present: string[];
      unlink: string[];
    }>;
  };
}

// The fixture is read at test time, never copied in: it is the authority both
// this package and the Rust crate render against, so a drift has to show up
// here as a failure rather than in a stale transcription.
const fixturePath = new URL(
  "../../../crates/subc-core/tests/fixtures/log_format_golden.json",
  import.meta.url,
);
  const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8")) as GoldenFixture;

// Complete CSI and OSC sequences, written independently of the renderer from
// the spec's wording. Rendering removes these, so a parsed-back line equals its
// event only once they are gone from the event's text too.
const COMPLETE_SEQUENCE =
  /(?:\u001b\[|\u009b)[\u0020-\u003f]*[\u0040-\u007e]|(?:\u001b\]|\u009d)[^]*?(?:\u0007|\u009c|\u001b\\)/g;

function withoutSequences(text: string): string {
  return text.replace(COMPLETE_SEQUENCE, "");
}

function withoutSequencesInPairs(
  pairs: ReadonlyArray<readonly [string, string | number | boolean]>,
): Array<[string, string | number | boolean]> {
  return pairs.map(([key, value]) => [key, typeof value === "string" ? withoutSequences(value) : value]);
}

const FIXED_MS = 1_788_604_863_123;
const FIXED_DAY = "2026-09-05";
const temporaryDirectories: string[] = [];

/**
 * Compares with the fixture case name inside the compared value, so a failure
 * report names the case that broke rather than only the two lines.
 */
function check(label: string, actual: unknown, expected: unknown): void {
  expect(`${label} => ${JSON.stringify(actual)}`).toBe(`${label} => ${JSON.stringify(expected)}`);
}

function temporaryDirectory(): string {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "cortexkit-log-"));
  temporaryDirectories.push(directory);
  return directory;
}

function configFor(logsDir: string, overrides: Partial<LogConfig> = {}): LogConfig {
  return {
    moduleId: "test-module",
    logsDir,
    clock: () => new Date(FIXED_MS),
    ...overrides,
  };
}

function segmentPath(logsDir: string, moduleId: string, day = FIXED_DAY): string {
  return path.join(logsDir, `${moduleId}.${day}.log`);
}

function readLines(file: string): string[] {
  const text = fs.readFileSync(file, "utf8");
  return text === "" ? [] : text.trimEnd().split("\n");
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
  for (const directory of temporaryDirectories.splice(0)) {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test("the fixture this suite pins to is the r2 schema", () => {
  check("fixture schema", fixture.schema, 2);
});

test("free-text message quotes do not open field values", () => {
  for (const message of ['he said "hi', 'he said "hi there"', 'he said "hi \\n']) {
    const event: LogEvent = { at_ms: FIXED_MS, level: "info", logger: "module", message, fields: [["count", 2]] };
    const parsed = parseLine(formatLine(event));
    expect(parsed).toMatchObject({ message, fields: [["count", "2"]] });
  }
});

test("a plain bracket in message text is not misplaced bound context", () => {
  const event: LogEvent = { at_ms: FIXED_MS, level: "info", logger: "module", message: "msg [x]", fields: [] };
  expect(parseLine(formatLine(event))).toMatchObject({ message: "msg [x]", fields: [] });
});

test("a trailing key=value in free text uses the documented field ambiguity", () => {
  const event: LogEvent = { at_ms: FIXED_MS, level: "info", logger: "module", message: "ends in a=b", fields: [] };
  expect(parseLine(formatLine(event))).toMatchObject({ message: "ends in", fields: [["a", "b"]] });
});

describe("golden line format", () => {
  for (const golden of fixture.cases) {
    test(`renders ${golden.name}`, () => {
      check(`render ${golden.name}`, formatLine(golden.event), golden.line);
    });

    test(`parses ${golden.name} back`, () => {
      const parsed = parseLine(golden.line);
      if ("reject" in parsed) throw new Error(`${golden.name}: rejected as ${parsed.reject}`);
      check(
        `parse ${golden.name}`,
        {
          at_ms: parsed.at_ms,
          level: parsed.level,
          logger: parsed.logger,
          bound: parsed.bound,
          message: parsed.message,
          fields: parsed.fields,
        },
        {
          at_ms: golden.event.at_ms,
          level: golden.event.level,
          logger: golden.event.logger,
          bound: withoutSequencesInPairs(golden.event.bound ?? []),
          message: withoutSequences(golden.event.message),
          fields: withoutSequencesInPairs(golden.event.fields),
        },
      );
    });
  }

  for (const rejected of fixture.parse_rejects) {
    test(`rejects ${rejected.name}`, () => {
      check(`reject ${rejected.name}`, parseLine(rejected.line), { reject: rejected.reason });
    });
  }

  test("the module id is the logger's first segment", () => {
    const parsed = parseLine(
      "2026-09-05T10:41:03.130Z WARN  magic-context.perf: [harness=opencode] folded ms=412",
    );
    if ("reject" in parsed) throw new Error(parsed.reject);
    check("module id", parsed.moduleId, "magic-context");
    check("session absent", parsed.session, null);
  });

  test("a bound session is split at its issuer prefix, whole", () => {
    const parsed = parseLine(
      "2026-09-05T10:41:05.000Z DEBUG magic-context.historian: " +
        "[session=pi:01a0b7fc-bba1-7f60-aa2b-27c2df4481ab] child spawned",
    );
    if ("reject" in parsed) throw new Error(parsed.reject);
    check("session", parsed.session, {
      issuer: "pi",
      id: "01a0b7fc-bba1-7f60-aa2b-27c2df4481ab",
    });
  });
});

describe("CK_LOG filtering", () => {
  for (const [index, filterCase] of fixture.level_filter.cases.entries()) {
    const label = `CK_LOG=${JSON.stringify(filterCase.spec)} ${filterCase.logger} ${filterCase.level}`;

    test(`case ${index}: ${label}`, async () => {
      const logsDir = temporaryDirectory();
      const [moduleId, ...components] = filterCase.logger.split(".");
      const captured = captureStderr(() => {
        let logger = createLogger(
          configFor(logsDir, { moduleId: moduleId!, spec: filterCase.spec }),
        );
        for (const component of components) logger = logger.child(component);
        return logger;
      });
      const logger = captured.result;

      check(`${label} enabled`, logger.enabled(filterCase.level), filterCase.emit);
      logger[filterCase.level]("probe");
      await logger.close();
      const written = readLines(segmentPath(logsDir, moduleId!));
      check(`${label} emitted`, written.length, filterCase.emit ? 1 : 0);
      if (filterCase.emit) {
        check(`${label} logger column`, written[0]?.includes(` ${filterCase.logger}: probe`), true);
      }

      if (filterCase.spec === "garbage=") {
        check(`${label} reported once`, captured.output.match(/invalid CK_LOG/g)?.length, 1);
        const second = captureStderr(() =>
          createLogger(
            configFor(temporaryDirectory(), { moduleId: moduleId!, spec: filterCase.spec }),
          ),
        );
        check(`${label} not reported twice`, second.output.includes("invalid CK_LOG"), false);
        await second.result.close();
      }
    });
  }

  test("an omitted spec reads CK_LOG from the environment", async () => {
    const logsDir = temporaryDirectory();
    const previous = process.env.CK_LOG;
    process.env.CK_LOG = "error";
    try {
      const logger = createLogger(configFor(logsDir));
      check("info suppressed by CK_LOG=error", logger.enabled("info"), false);
      check("error allowed by CK_LOG=error", logger.enabled("error"), true);
      await logger.close();
    } finally {
      if (previous === undefined) delete process.env.CK_LOG;
      else process.env.CK_LOG = previous;
    }
  });
});

describe("segment names", () => {
  for (const segmentCase of fixture.segment_name.cases) {
    const label = `${segmentCase.module} at ${segmentCase.at_ms}`;

    test(`names ${segmentCase.name}`, () => {
      check(label, segmentName(segmentCase.module, new Date(segmentCase.at_ms)), segmentCase.name);
    });

    test(`writes into ${segmentCase.name}`, async () => {
      const logsDir = temporaryDirectory();
      const logger = createLogger(
        configFor(logsDir, {
          moduleId: segmentCase.module,
          clock: () => new Date(segmentCase.at_ms),
        }),
      );
      logger.info("segment probe");
      await logger.close();
      check(`${label} directory`, fs.readdirSync(logsDir), [segmentCase.name]);
    });
  }

  test("a day roll opens the next segment and renames nothing", async () => {
    const logsDir = temporaryDirectory();
    let now = new Date("2026-09-05T23:59:59.999Z");
    const logger = createLogger(
      configFor(logsDir, { moduleId: "engram", clock: () => now }),
    );

    logger.info("last line of the fifth");
    now = new Date("2026-09-06T00:00:00.000Z");
    logger.info("first line of the sixth");
    await logger.close();

    const fifth = segmentPath(logsDir, "engram", "2026-09-05");
    const sixth = segmentPath(logsDir, "engram", "2026-09-06");
    check("day roll directory", fs.readdirSync(logsDir).sort(), [
      "engram.2026-09-05.log",
      "engram.2026-09-06.log",
    ]);
    check("fifth holds only its own line", readLines(fifth).length, 1);
    expect(readLines(fifth)[0]).toContain("last line of the fifth");
    expect(readLines(sixth)[0]).toContain("first line of the sixth");
  });

  test("two writers with different harness bindings share one segment", async () => {
    const logsDir = temporaryDirectory();
    const clock = () => new Date(FIXED_MS);
    const pi = createLogger({ ...forPlugin("magic-context", "pi"), logsDir, clock });
    const opencode = createLogger({ ...forPlugin("magic-context", "opencode"), logsDir, clock });

    const burst = async (logger: Logger): Promise<void> => {
      for (let index = 0; index < 300; index += 1) {
        logger.info("burst", { index });
        await Promise.resolve();
      }
    };
    await Promise.all([burst(pi), burst(opencode)]);
    await Promise.all([pi.close(), opencode.close()]);

    const lines = readLines(segmentPath(logsDir, "magic-context"));
    const counts: Record<string, number> = { pi: 0, opencode: 0 };
    for (const line of lines) {
      const parsed = parseLine(line);
      if ("reject" in parsed) throw new Error(`unparsable line (${parsed.reject}): ${line}`);
      const harness = parsed.bound.find(([key]) => key === "harness")?.[1];
      if (harness === undefined) throw new Error(`line without harness binding: ${line}`);
      counts[harness] = (counts[harness] ?? 0) + 1;
    }
    check("shared segment line count", lines.length, 600);
    check("harness counts", counts, { pi: 300, opencode: 300 });
  });
});

describe("retention", () => {
  for (const pruneCase of fixture.retention_prune.cases) {
    test(`selects candidates for ${pruneCase.name}`, () => {
      const candidates = pruneCandidates(
        pruneCase.module,
        pruneCase.today,
        pruneCase.max_age_days,
        pruneCase.present,
      );
      check(`candidates ${pruneCase.name}`, candidates.slice().sort(), pruneCase.unlink.slice().sort());
    });

    test(`unlinks exactly those files for ${pruneCase.name}`, async () => {
      const logsDir = temporaryDirectory();
      for (const name of pruneCase.present) {
        fs.writeFileSync(path.join(logsDir, name), `${name}\n`);
      }
      const logger = createLogger(
        configFor(logsDir, {
          moduleId: pruneCase.module,
          maxAgeDays: pruneCase.max_age_days,
          clock: () => new Date(`${pruneCase.today}T12:00:00.000Z`),
        }),
      );
      await logger.close();

      const survivors = new Set(
        pruneCase.present.filter((name) => !pruneCase.unlink.includes(name)),
      );
      survivors.add(`${pruneCase.module}.${pruneCase.today}.log`);
      check(
        `survivors ${pruneCase.name}`,
        fs.readdirSync(logsDir).sort(),
        [...survivors].sort(),
      );
    });
  }

  test("a name that is not this module's segment has no day", () => {
    check("foreign module", segmentDay("engram", "other-module.2026-08-01.log"), null);
    check("own segment", segmentDay("engram", "engram.2026-08-01.log"), "2026-08-01");
  });

  test("retention at open announces what it pruned and leaves foreign names alone", async () => {
    const logsDir = temporaryDirectory();
    const survivors = [
      "magic-context.2026-09-19.log",
      "magic-context.2026-09-05.log",
      // A pid-suffixed file and a per-harness file from the previous format:
      // neither parses as a segment, so neither is a prune candidate.
      "magic-context-10004.log",
      "magic-context.opencode.log",
      "other-module.2026-09-01.log",
    ];
    const doomed = ["magic-context.2026-09-04.log", "magic-context.2026-08-01.log"];
    for (const name of [...survivors, ...doomed]) {
      fs.writeFileSync(path.join(logsDir, name), `${name}\n`);
    }

    const captured = captureStderr(() =>
      createLogger(
        configFor(logsDir, {
          moduleId: "magic-context",
          maxAgeDays: 14,
          clock: () => new Date("2026-09-19T08:00:00.000Z"),
        }),
      ),
    );
    await captured.result.close();

    check(
      "retention announcement",
      captured.output.trimEnd(),
      "@cortexkit/log: magic-context.retention pruned=2 kept=2",
    );
    check("survivors", fs.readdirSync(logsDir).sort(), survivors.slice().sort());
  });

  test("the window falls back to CK_LOG_MAX_AGE_DAYS when the config omits it", async () => {
    const logsDir = temporaryDirectory();
    fs.writeFileSync(path.join(logsDir, "test-module.2026-09-01.log"), "aged\n");
    fs.writeFileSync(path.join(logsDir, "test-module.2026-09-03.log"), "inside the window\n");
    const previous = process.env.CK_LOG_MAX_AGE_DAYS;
    process.env.CK_LOG_MAX_AGE_DAYS = "2";
    try {
      const logger = createLogger(configFor(logsDir));
      await logger.close();
      check("survivors", fs.readdirSync(logsDir).sort(), [
        "test-module.2026-09-03.log",
        `test-module.${FIXED_DAY}.log`,
      ]);
    } finally {
      if (previous === undefined) delete process.env.CK_LOG_MAX_AGE_DAYS;
      else process.env.CK_LOG_MAX_AGE_DAYS = previous;
    }
  });

  test("an oversized segment is reported once and never truncated", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(configFor(logsDir, { alarmSegmentMb: 1 / 1024 }));
    const captured = captureStderr(() => {
      for (let index = 0; index < 40; index += 1) logger.info("oversize probe", { index });
    });
    await logger.close();

    check("alarm reported once", captured.output.match(/segment oversized/g)?.length, 1);
    expect(captured.output).toContain("NOT truncated");
    check("nothing truncated", readLines(segmentPath(logsDir, "test-module")).length, 40);
  });
});

describe("bound fields", () => {
  test("nothing bound renders no bracket", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(configFor(logsDir));
    logger.info("plain");
    await logger.close();

    const line = readLines(segmentPath(logsDir, "test-module"))[0] ?? "";
    check("no bracket", line.includes("["), false);
    expect(line).toEndWith(" test-module: plain");
  });

  test("process-level fields render first and an inner scope overrides in place", async () => {
    const logsDir = temporaryDirectory();
    const clock = () => new Date(FIXED_MS);
    const logger = createLogger({ ...forPlugin("magic-context", "pi"), logsDir, clock });
    const scoped = logger
      .withBound([
        ["session", "pi:01a0b7fc"],
        ["root", "/Users/x/My Project"],
      ])
      .withBound({ harness: "opencode" });

    scoped.info("scoped");
    logger.info("process only");
    await logger.close();

    const [scopedLine, processLine] = readLines(segmentPath(logsDir, "magic-context"));
    expect(scopedLine).toContain(
      '[harness=opencode session=pi:01a0b7fc root="/Users/x/My Project"] scoped',
    );
    expect(processLine).toContain("[harness=pi] process only");
  });

  test("withSession is sugar for the session binding and empty halves bind nothing", async () => {
    const logsDir = temporaryDirectory();
    const clock = () => new Date(FIXED_MS);
    const logger = createLogger({ ...forPlugin("magic-context", "pi"), logsDir, clock });

    logger.withSession("opencode", "ses_0758f6ce7ffeJ0A9sV8Qvema7d").info("with session");
    logger.withSession("", "ses_orphan").info("without issuer");
    logger.withSession("opencode", "").info("without id");
    await logger.close();

    const lines = readLines(segmentPath(logsDir, "magic-context"));
    expect(lines[0]).toContain(
      "[harness=pi session=opencode:ses_0758f6ce7ffeJ0A9sV8Qvema7d] with session",
    );
    expect(lines[1]).toContain("[harness=pi] without issuer");
    expect(lines[2]).toContain("[harness=pi] without id");
  });

  test("child appends a component to the logger name", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(configFor(logsDir, { moduleId: "magic-context" }));

    logger.child("historian").info("one level");
    logger.child("wan").child("mapping").info("two levels");
    await logger.close();

    const lines = readLines(segmentPath(logsDir, "magic-context"));
    expect(lines[0]).toContain(" magic-context.historian: one level");
    expect(lines[1]).toContain(" magic-context.wan.mapping: two levels");
    expect(() => logger.child("Perf")).toThrow("logger component");
  });

  test("a module id outside the segment grammar is refused at construction", () => {
    expect(() => createLogger(configFor(temporaryDirectory(), { moduleId: "Magic-Context" }))).toThrow(
      "module id",
    );
  });

  test("forPlugin binds the harness and nothing else", () => {
    check("forPlugin config", forPlugin("magic-context", "opencode"), {
      moduleId: "magic-context",
      bound: [["harness", "opencode"]],
    });
  });
});

describe("files", () => {
  test("a module and its plugin share one module-owned segment", async () => {
    const dataHome = temporaryDirectory();
    const previous = process.env.XDG_DATA_HOME;
    process.env.XDG_DATA_HOME = dataHome;
    try {
      const clock = () => new Date(FIXED_MS);
      const moduleLogger = createLogger({ moduleId: "magic-context", clock });
      const pluginLogger = createLogger({
        ...forPlugin("magic-context", "opencode"),
        clock,
      });

      moduleLogger.info("from the module");
      pluginLogger.info("from the plugin");
      await Promise.all([moduleLogger.close(), pluginLogger.close()]);

      const logs = path.join(dataHome, "cortexkit", "magic-context", "logs");
      const file = segmentPath(logs, "magic-context");
      const lines = readLines(file);
      check("one file", fs.readdirSync(logs), ["magic-context.2026-09-05.log"]);
      check("both lanes in it", lines.length, 2);
      expect(lines[0]).toContain("magic-context: from the module");
      expect(lines[1]).toContain("[harness=opencode] from the plugin");
      if (process.platform !== "win32") {
        check("directory mode", fs.statSync(logs).mode & 0o777, 0o700);
        check("file mode", fs.statSync(file).mode & 0o777, 0o600);
      }
    } finally {
      if (previous === undefined) delete process.env.XDG_DATA_HOME;
      else process.env.XDG_DATA_HOME = previous;
    }
  });

  test("an open failure announces stderr fallback on its first line", async () => {
    const directory = temporaryDirectory();
    const blocker = path.join(directory, "not-a-directory");
    fs.writeFileSync(blocker, "blocker");
    const logsDir = path.join(blocker, "logs");

    const captured = captureStderr(() => {
      const logger = createLogger(configFor(logsDir));
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
      logsDir,
      path: segmentPath(logsDir, "test-module"),
    });
  });
});

describe("redaction and the one-line contract", () => {
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
    test(`redacts ${name}`, async () => {
      const logsDir = temporaryDirectory();
      const logger = createLogger(configFor(logsDir));
      logger.info(`credential ${value}`);
      await logger.close();

      const line = readLines(segmentPath(logsDir, "test-module"))[0] ?? "";
      expect(line).toContain("[REDACTED]");
      expect(line).not.toContain(secret);
    });
  }

  // Every case against the redactor itself, as the Rust twin tests it: the
  // fixture's inputs are rendered lines, and some (an escaped quote inside a
  // quoted value) cannot be produced by rendering a message verbatim.
  for (const redaction of fixture.redaction.cases) {
    test(`fleet redactor: ${redaction.name}`, () => {
      check(`redact ${redaction.name}`, fleetRedact(redaction.input), redaction.output);
    });
  }

  // And once through a real logger, so the wiring from write to redactor is
  // exercised on a rendered line exactly as in production.
  test("the logger applies the fleet redactor to the rendered line", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(configFor(logsDir));
    logger.info("GET", { url: "https://api.example.com/v1?access_token=abc123&page=2" });
    await logger.close();
    const lines = readLines(segmentPath(logsDir, "test-module"));
    check("one line", lines.length, 1);
    expect(lines[0]).toEndWith("GET url=https://api.example.com/v1?access_token=[REDACTED]&page=2");
  });

  test("an unterminated OSC in one value does not swallow the fields after it", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(configFor(logsDir));
    logger.warn("server said", { text: "x\u001b]title", code: 2 });
    await logger.close();

    const lines = readLines(segmentPath(logsDir, "test-module"));
    check("one line", lines.length, 1);
    const parsed = parseLine(lines[0] ?? "");
    if ("reject" in parsed) throw new Error(`rejected as ${parsed.reject}: ${lines[0]}`);
    check("fields survive", parsed.fields, [
      ["text", "x\u001b]title"],
      ["code", "2"],
    ]);
    expect(lines[0]).toEndWith(' text="x\\u001b]title" code=2');
  });

  test("ordinary text passes through and caller redaction runs second", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(
      configFor(logsDir, { redact: (line) => line.replace("customer-name", "[CUSTOM]") }),
    );
    logger.info("ordinary text customer-name");
    await logger.close();

    const line = readLines(segmentPath(logsDir, "test-module"))[0] ?? "";
    expect(line).toContain("ordinary text [CUSTOM]");
    expect(line).not.toContain("customer-name");
  });

  test("a caller redactor cannot split one event across two lines", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(
      configFor(logsDir, { redact: (line) => line.replace("split", "one\ntwo") }),
    );
    logger.info("split here");
    await logger.close();

    const lines = readLines(segmentPath(logsDir, "test-module"));
    check("still one line", lines.length, 1);
    expect(lines[0]).toContain("one\\ntwo here");
  });

  test("ANSI sequences in fields are removed and counted; a redactor's are escaped", async () => {
    const logsDir = temporaryDirectory();
    const logger = createLogger(
      configFor(logsDir, { redact: (line) => line.replace("plain", "\u001b[1mbold\u001b[0m") }),
    );
    logger.info("color \u001b[31mred\u001b[0m and plain", { tag: "\u001b[32mok\u001b[0m" });
    await logger.close();

    const line = readLines(segmentPath(logsDir, "test-module"))[0] ?? "";
    expect(line).not.toContain("\u001b");
    // The guard after the redactor never strips, so what a redactor introduces
    // is kept as visible `\u001b` text rather than removed.
    expect(line).toEndWith("color red and \\u001b[1mbold\\u001b[0m tag=ok");
    check("sequences stripped", logger.stats().ansiStripped, 4);
  });
});

test("write failures are swallowed and reported once", async () => {
  const logsDir = temporaryDirectory();
  let now = new Date(FIXED_MS);
  const logger = createLogger(configFor(logsDir, { clock: () => now }));
  logger.info("opens successfully");

  // The directory disappears and the clock crosses midnight, so the next write
  // has to reopen a segment in a directory that is no longer there.
  fs.rmSync(logsDir, { recursive: true, force: true });
  now = new Date(FIXED_MS + 24 * 60 * 60 * 1_000);
  const captured = captureStderr(() => {
    expect(() => logger.info("first failed write")).not.toThrow();
    expect(() => logger.info("second failed write")).not.toThrow();
  });
  await logger.close();

  check("swallowed", logger.stats().swallowedWrites, 2);
  check("reported once", captured.output.match(/log write failed/g)?.length, 1);
});
