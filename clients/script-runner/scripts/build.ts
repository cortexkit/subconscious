import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

if (Bun.version !== "1.4.4") throw new Error(`Build requires Bun 1.4.4, found ${Bun.version}`);
mkdirSync("dist", { recursive: true });
const output = process.env.RUNNER_OUTFILE ?? resolve("dist", process.platform === "win32" ? "ck-script-runner-bun.exe" : "ck-script-runner-bun");
const child = Bun.spawn([
  process.execPath, "build", "--compile",
  "--no-compile-autoload-bunfig", "--no-compile-autoload-dotenv",
  "--no-compile-autoload-tsconfig", "--no-compile-autoload-package-json",
  ...(process.env.BUN_COMPILE_TARGET ? ["--target", process.env.BUN_COMPILE_TARGET] : []),
  "--outfile", output, "src/main.ts",
], { stdout: "inherit", stderr: "inherit" });
process.exit(await child.exited);
