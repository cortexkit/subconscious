import { appendFileSync } from "node:fs";
import { managementSurfaceManifest, SubcProvider } from "@cortexkit/subc-client";

appendFileSync(process.env.SCRIPT_MODULE_EVENTS!, `${process.pid}\n`);
const provider = await SubcProvider.connect({
  connectionFile: process.env.SUBC_CONNECTION_FILE!,
  manifest: managementSurfaceManifest({
    moduleId: process.env.SUBC_MODULE_ID!,
    operations: ["echo"],
  }),
  handler: async (_channel, body) => Buffer.from(JSON.stringify({
    answer: "from the compiled script runner",
    request: JSON.parse(Buffer.from(body).toString()),
    pid: process.pid,
    executable: process.execPath,
  })),
});
await provider.closed;
