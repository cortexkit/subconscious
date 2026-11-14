import { expect, test } from "bun:test";
import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type AddressInfo, type Socket } from "node:net";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

test("bun build --compile embeds the registration helper", async () => {
  mkdirSync(resolve("target/scratch"), {recursive: true});
  const dir = mkdtempSync(resolve("target/scratch/compiled-registration-"));
  const sdk = join(dir, "sdk");
  const sockets: Socket[] = [];
  const server = createServer(socket => { sockets.push(socket); socket.write(Buffer.from([42])); });
  const env = {...process.env};
  for (const name of Object.keys(env)) if (name.startsWith("SUBC_")) delete env[name];
  try {
    cpSync(fileURLToPath(new URL("../src", import.meta.url)), join(sdk, "src"), {recursive: true});
    cpSync(fileURLToPath(new URL("../native", import.meta.url)), join(sdk, "native"), {recursive: true});
    writeFileSync(join(dir, "module.ts"), `
      import { ClaimError } from "./sdk/src/index.ts";
      import { registrationOwner } from "./sdk/src/registration-channel.ts";
      const owner = registrationOwner();
      owner.adopt();
      if (!owner.supervised) throw new ClaimError("helper_not_loaded");
      // Wait for the launcher to verify the Windows pipe client before exiting.
      const ready = await owner.take()!.readExact(1, Date.now() + 10000);
      if (ready[0] !== 42) throw new Error("registration channel did not carry data");
      console.log("COMPILED_HELPER_LOADED");
      process.exit(0);
    `);
    const executable = join(dir, process.platform === "win32" ? "module.exe" : "module");
    const build = Bun.spawn([process.execPath, "build", "--compile", join(dir, "module.ts"), "--outfile", executable], {stdout: "pipe", stderr: "pipe"});
    const [buildCode, buildOut, buildErr] = await Promise.all([build.exited, new Response(build.stdout).text(), new Response(build.stderr).text()]);
    expect({code: buildCode, diagnostics: buildCode === 0 ? "" : buildOut + buildErr}).toEqual({code: 0, diagnostics: ""});
    // Only the executable survives: no checkout-relative native file can rescue it.
    rmSync(sdk, {recursive: true});
    rmSync(join(dir, "module.ts"));
    await new Promise<void>(r => server.listen(0, "127.0.0.1", r));
    const launcher = fileURLToPath(new URL("fixtures/registration-launcher.py", import.meta.url));
    const child = Bun.spawn([process.platform === "win32" ? "python" : "python3", launcher,
      String((server.address() as AddressInfo).port), executable, "compiled", "compiled"],
      {cwd: dir, env, stdout: "pipe", stderr: "pipe"});
    const timer = setTimeout(() => child.kill(), 15000);
    try {
      const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
      expect({code, err}).toEqual({code: 0, err: ""});
      expect(out).toBe("COMPILED_HELPER_LOADED\n");
    } finally { clearTimeout(timer); }
  } finally {
    for (const socket of sockets) socket.destroy();
    server.close();
    // Windows can keep a just-exited executable locked for a moment.
    rmSync(dir, {recursive: true, force: true, maxRetries: 20, retryDelay: 100});
  }
}, 60000);
