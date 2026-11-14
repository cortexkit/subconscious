import { readFileSync } from "node:fs";
import { check, evaluate } from "./lock.ts";
import { readScript } from "./pipe.ts";
import { registrationHelper } from "./native.ts";
import { pathWarnings } from "./warnings.ts";

try {
  for (const name of Object.keys(process.env)) {
    const key = process.platform === "win32" ? name.toUpperCase() : name;
    if (["NODE_OPTIONS", "BUN_OPTIONS", "BUN_INSTALL", "NODE_PATH"].includes(key) || key.startsWith("BUN_CONFIG_")) {
      throw new Error(`script_environment_refused: ${name}`);
    }
  }
  const args = process.argv.slice(2);
  if (args.length === 2 && args[0] === "--check") {
    const source = readFileSync(args[1], "utf8");
    for (const warning of pathWarnings(source)) console.error(warning);
    const refused = check(source);
    for (const specifier of refused) console.error(`script_import_refused: ${specifier}`);
    process.exitCode = refused.length ? 1 : 0;
  } else {
    const source = await readScript();
    await evaluate(source, registrationHelper());
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
