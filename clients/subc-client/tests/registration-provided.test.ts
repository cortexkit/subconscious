import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

test("registration uses the runner-provided helper before native files", async () => {
  mkdirSync("target/scratch", { recursive: true });
  const dir = mkdtempSync(resolve("target/scratch/provided-registration-"));
  const entry = join(dir, "provided.ts");
  writeFileSync(entry, `
    import { plugin } from "bun";
    plugin({name: "provided-registration", setup(build) {
      build.onLoad({filter: /^registration$/, namespace: "ck"}, () => ({
        loader: "object", exports: { supervised() { console.log("PROVIDED_HELPER_USED"); return false; } }
      }));
    }});
    const { registrationOwner } = await import(${JSON.stringify(fileURLToPath(new URL("../src/registration-channel.ts", import.meta.url)))});
    registrationOwner().adopt();
  `);
  try {
    const child = Bun.spawn([process.execPath, entry], {
      env: { ...process.env, SUBC_REGISTRATION_FD: "999:1", SUBC_REGISTRATION_PIPE: "not-a-real-pipe" }, stdout: "pipe", stderr: "pipe",
    });
    const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    expect({ code, out, err }).toEqual({ code: 0, out: "PROVIDED_HELPER_USED\n", err: "" });
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("runner export condition fails closed without the provided helper", async () => {
  const channel = fileURLToPath(new URL("../src/registration-channel.ts", import.meta.url));
  const script = `
    const {registrationOwner} = await import(${JSON.stringify(channel)});
    try { registrationOwner().adopt(); throw new Error("unexpected native fallback"); }
    catch (error) {
      if (error.code !== "registration_helper_unavailable") throw error;
      console.log("PROVIDED_HELPER_REQUIRED");
    }
  `;
  const child = Bun.spawn([process.execPath, "--conditions=ck-script-runner", "-e", script], {
    env: { ...process.env, SUBC_REGISTRATION_FD: "999:1", SUBC_REGISTRATION_PIPE: "not-a-real-pipe" }, stdout: "pipe", stderr: "pipe",
  });
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect({ code, out, err }).toEqual({ code: 0, out: "PROVIDED_HELPER_REQUIRED\n", err: "" });
});
