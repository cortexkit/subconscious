import { appendFileSync, readFileSync } from "node:fs";
import { managementSurfaceManifest, SubcProvider } from "../../src/index.js";

const file = process.env.SAMPLE_FILE!;
const events = process.env.SAMPLE_EVENTS!;
const load = () => JSON.parse(readFileSync(file, "utf8")) as { limit: number };
let current: { limit: number } | undefined = load();
const provider: SubcProvider = await SubcProvider.connect({
  connectionFile: process.env.SUBC_CONNECTION_FILE!,
  manifest: managementSurfaceManifest({ moduleId: process.env.SUBC_MODULE_ID!, operations: ["read"] }),
  handler: async () => { current = load(); return Buffer.from(JSON.stringify(current)); },
});
appendFileSync(events, JSON.stringify({ started: true, current }) + "\n");
await provider.closed;
