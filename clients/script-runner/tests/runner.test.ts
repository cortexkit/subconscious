import { afterAll, beforeAll, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { resolve, join } from "node:path";
import { removeScratch } from "./remove-scratch";

let dir: string;
let executable: string;
const env = { ...process.env };
for (const name of Object.keys(env)) {
  const key = process.platform === "win32" ? name.toUpperCase() : name;
  if (["NODE_OPTIONS", "BUN_OPTIONS", "BUN_INSTALL", "NODE_PATH"].includes(key) || key.startsWith("BUN_CONFIG_")) delete env[name];
}

beforeAll(async () => {
  mkdirSync("target/scratch", { recursive: true });
  dir = mkdtempSync(resolve("target/scratch/runner-"));
  executable = join(dir, process.platform === "win32" ? "runner.exe" : "runner");
  writeFileSync(join(dir, "refused.cjs"), 'console.log("DEPENDENCY_EVALUATED"); module.exports = {};');
  const build = Bun.spawn([process.execPath, "scripts/build.ts"], {
    env: { ...env, RUNNER_OUTFILE: executable }, stdout: "pipe", stderr: "pipe",
  });
  const [code, out, err] = await Promise.all([build.exited, new Response(build.stdout).text(), new Response(build.stderr).text()]);
  expect({ code, diagnostics: code ? out + err : "" }).toEqual({ code: 0, diagnostics: "" });
}, 60000);

afterAll(() => removeScratch(dir));

test("version probe runs without a script pipe", async () => {
  const child = Bun.spawn([executable, "--version"], { env, stdout: "pipe", stderr: "pipe" });
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect({ code, out, err }).toEqual({ code: 0, out: "ck-script-runner-bun 0.1.0 (Bun 1.4.4)\n", err: "" });
});

// This is also the daemon's wire layout: magic/version/length/digest, then bytes.
function packet(source: string): Buffer {
  const bytes = Buffer.from(source);
  const header = Buffer.alloc(52);
  header.write("CKSCRIPT");
  header.writeUInt32LE(1, 8);
  header.writeBigUInt64LE(BigInt(bytes.length), 12);
  createHash("sha256").update(bytes).digest().copy(header, 20);
  return Buffer.concat([header, bytes]);
}

async function run(bytes: Buffer, scenario = "valid", environment = env) {
  const child = Bun.spawn([process.platform === "win32" ? "python" : "python3", "tests/pipe-launcher.py", executable, scenario], {
    env: environment, stdin: "pipe", stdout: "pipe", stderr: "pipe",
  });
  child.stdin.write(bytes);
  child.stdin.end();
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect({ code, err }).toEqual({ code: 0, err: "" });
  return JSON.parse(out) as { code: number; out: string; err: string; write_errors: string[] };
}

test("CKSCRIPT header matches an independent Python wire vector", async () => {
  // Python struct '<8sIQ32s' and hashlib generated this fixture, not packet().
  const vector = await Bun.file("tests/script-pipe-vector.json").json() as { source: string; header_hex: string };
  expect(packet(vector.source).subarray(0, 52).toString("hex")).toBe(vector.header_hex);
  const result = await run(Buffer.concat([Buffer.from(vector.header_hex, "hex"), Buffer.from(vector.source)]));
  expect(result).toEqual({ code: 0, out: "PIPE_OK\n", err: "", write_errors: [] });
});

test("verified bytes evaluate as ESM with top-level await and provided registration", async () => {
  const result = await run(packet(`
    import { supervised } from "ck:registration";
    import { basename } from "node:path";
    import { existsSync, statSync } from "node:fs";
    await Promise.resolve();
    console.log(import.meta.url, basename("/ok"), typeof supervised, typeof import.meta.dir, existsSync(__dirname) && statSync(__dirname).isDirectory());
  `));
  expect(result).toEqual({ code: 0, out: "ck-script:main ok function undefined false\n", err: "", write_errors: [] });
});

for (const specifier of ["./relative.js", "npm-package", "./addon.node"]) {
  test(`refuses ${specifier} before any script code runs`, async () => {
    const result = await run(packet(`console.log("EVALUATED"); import ${JSON.stringify(specifier)};`));
    expect(result.code).not.toBe(0);
    expect(result.out).toBe("");
    expect(result.err).toContain(`script_import_refused: ${specifier}`);
  });
}

for (const expression of [
  "await import(process.env.REFUSED_SPECIFIER)",
  "require(process.env.REFUSED_SPECIFIER)",
  "import.meta.require(process.env.REFUSED_SPECIFIER)",
  'createRequire(process.env.REQUIRE_BASE)(process.env.REFUSED_SPECIFIER)',
]) {
  test(`runtime lock refuses ${expression}`, async () => {
    const result = await run(packet(`import { createRequire } from "node:module"; ${expression}; console.log("EVALUATED");`), "valid", {
      ...env, REFUSED_SPECIFIER: join(dir, "refused.cjs"), REQUIRE_BASE: join(dir, "entry.js"),
    });
    expect(result.code).not.toBe(0);
    expect(result.out).toBe("");
    expect(result.err).toContain("script_import_refused:");
  });
}

test("runtime lock labels a computed absent package refusal", async () => {
  const result = await run(packet('await import(process.env.REFUSED_SPECIFIER); console.log("EVALUATED");'), "valid", {
    ...env, REFUSED_SPECIFIER: "absent-refused-package",
  });
  expect(result.code).not.toBe(0);
  expect(result.out).toBe("");
  expect(result.err).toContain("script_import_refused:");
  expect(result.err).toContain("absent-refused-package");
});

test("an author bundle uses the provided SDK helper without loading native files", async () => {
  const entry = join(dir, "sdk-entry.ts");
  const bundle = join(dir, "sdk-bundle.js");
  writeFileSync(entry, `
    import { SubcClient } from ${JSON.stringify(resolve("../subc-client/src/index.ts"))};
    import { registrationOwner } from ${JSON.stringify(resolve("../subc-client/src/registration-channel.ts"))};
    process.env.SUBC_REGISTRATION_FD = "999:1";
    process.env.SUBC_REGISTRATION_PIPE = "invalid";
    const owner = registrationOwner();
    owner.adopt();
    console.log("SDK_HELPER_CALLED", owner.supervised, typeof SubcClient);
  `);
  const build = Bun.spawn([process.execPath, "build", "--target=bun", "--format=esm", "--conditions=ck-script-runner", "--outfile", bundle, entry], {
    env, stdout: "pipe", stderr: "pipe",
  });
  const [code, out, err] = await Promise.all([build.exited, new Response(build.stdout).text(), new Response(build.stderr).text()]);
  expect({ code, diagnostics: code ? out + err : "" }).toEqual({ code: 0, diagnostics: "" });
  const source = await Bun.file(bundle).text();
  expect(source).not.toContain(".node");
  const checked = Bun.spawn([executable, "--check", bundle], { env, stdout: "pipe", stderr: "pipe" });
  expect(await checked.exited).toBe(0);
  expect(await new Response(checked.stderr).text()).toBe("");
  const result = await run(packet(source));
  expect(result.code).toBe(0);
  expect(result.out).toContain("SDK_HELPER_CALLED");
  expect(result.err).toBe("");
});

test("without the runner condition check mode reports SDK native fallbacks", async () => {
  const entry = join(dir, "ordinary-sdk.ts");
  const output = join(dir, "ordinary-bundle");
  writeFileSync(entry, `import { SubcClient } from ${JSON.stringify(resolve("../subc-client/src/index.ts"))}; console.log(SubcClient);`);
  const build = Bun.spawn([process.execPath, "build", "--target=bun", "--format=esm", "--outdir", output, entry], {
    env, stdout: "pipe", stderr: "pipe",
  });
  const [code, out, err] = await Promise.all([build.exited, new Response(build.stdout).text(), new Response(build.stderr).text()]);
  expect({ code, diagnostics: code ? out + err : "" }).toEqual({ code: 0, diagnostics: "" });
  const child = Bun.spawn([executable, "--check", join(output, "ordinary-sdk.js")], { env, stdout: "pipe", stderr: "pipe" });
  const [checked, diagnostics] = await Promise.all([child.exited, new Response(child.stderr).text()]);
  expect(checked).toBe(1);
  expect(diagnostics).toContain("script_import_refused:");
  expect(diagnostics).toContain(".node");
});

test("check mode warns on free path globals with line numbers but not local names", async () => {
  const file = join(dir, "warnings.js");
  writeFileSync(file, 'console.log(__dirname);\nconsole.log(__filename);\nfunction local(__dirname) { return __dirname; }\nconsole.log({__dirname});\nconsole.log({__dirname: "property"});');
  const child = Bun.spawn([executable, "--check", file], { env, stdout: "pipe", stderr: "pipe" });
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect(code).toBe(0);
  expect(out).toBe("");
  expect(err).toContain("__dirname at line 1");
  expect(err).toContain("__filename at line 2");
  expect(err).toContain("__dirname at line 4");
  expect(err).not.toContain("at line 3");
  expect(err).not.toContain("at line 5");
});

for (const variable of ["NODE_OPTIONS", "BUN_OPTIONS", "BUN_INSTALL", "NODE_PATH", "BUN_CONFIG_TEST", ...(process.platform === "win32" ? ["Node_Options"] : [])]) {
  test(`refuses loader environment ${variable}`, async () => {
    const result = await run(packet('console.log("EVALUATED")'), "valid", { ...env, [variable]: "" });
    expect(result.code).not.toBe(0);
    expect(result.out).toBe("");
    expect(result.err.toUpperCase()).toContain(`SCRIPT_ENVIRONMENT_REFUSED: ${variable.toUpperCase()}`);
  });
}

for (const fault of ["wrong-identity", "short-header", "short-body", "extra-byte", "digest", "magic", "version", "oversized", ...(process.platform !== "win32" ? ["not-pipe"] : ["wrong-name"])]) {
  test(`transport refuses ${fault} before evaluation`, async () => {
    let bytes = packet('console.log("EVALUATED")');
    if (fault === "short-header") bytes = bytes.subarray(0, 51);
    if (fault === "short-body") bytes = bytes.subarray(0, -1);
    if (fault === "extra-byte") bytes = Buffer.concat([bytes, Buffer.from([0])]);
    if (fault === "digest") bytes[20] ^= 1;
    if (fault === "magic") bytes[0] ^= 1;
    if (fault === "version") bytes.writeUInt32LE(2, 8);
    if (fault === "oversized") bytes = packet(`/*${"x".repeat(32 * 1024 * 1024)}*/console.log("EVALUATED")`);
    const result = await run(bytes, fault);
    expect(result.code).not.toBe(0);
    expect(result.out).toBe("");
    expect(result.err).toContain("script_pipe_refused:");
    // The oversized case pushes 32 MiB through the Python launcher's pipe,
    // which takes longer than Bun's 5-second default on slow runners.
  }, 30000);
}

test("check mode lists every refused literal without evaluation", async () => {
  const file = join(dir, "check.js");
  writeFileSync(file, 'console.log("EVALUATED"); import "./a.js"; import "some-package"; require("./addon.node");');
  const child = Bun.spawn([executable, "--check", file], { env, stdout: "pipe", stderr: "pipe" });
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect(code).toBe(1);
  expect(out).toBe("");
  for (const specifier of ["./a.js", "some-package", "./addon.node"]) expect(err).toContain(`script_import_refused: ${specifier}`);
});

test("check mode accepts builtins and registration without evaluation", async () => {
  const file = join(dir, "clean.js");
  writeFileSync(file, 'console.log("EVALUATED"); import "node:fs"; import "bun"; import "ck:registration";');
  const child = Bun.spawn([executable, "--check", file], { env, stdout: "pipe", stderr: "pipe" });
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect({ code, out, err }).toEqual({ code: 0, out: "", err: "" });
});

for (const config of ["bunfig.toml", ".env"]) {
  test(`compiled runner ignores working-folder ${config}`, async () => {
    const cwd = mkdtempSync(join(dir, "config-"));
    writeFileSync(join(cwd, config), config === "bunfig.toml" ? "not valid TOML!" : "NODE_OPTIONS=untrusted\nBUN_CONFIG_TEST=untrusted\n");
    const file = join(cwd, "config-check.js");
    writeFileSync(file, 'throw new Error("EVALUATED")');
    const child = Bun.spawn([executable, "--check", file], { cwd, env, stdout: "pipe", stderr: "pipe" });
    const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    expect({ code, out, err }).toEqual({ code: 0, out: "", err: "" });
  });
}

test("transport refuses invalid UTF-8 before evaluation", async () => {
  const bytes = packet('/*x*/console.log("EVALUATED")');
  bytes[54] = 0xff;
  createHash("sha256").update(bytes.subarray(52)).digest().copy(bytes, 20);
  const result = await run(bytes);
  expect(result.code).not.toBe(0);
  expect(result.out).toBe("");
});
