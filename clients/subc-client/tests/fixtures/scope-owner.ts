import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { managementSurfaceManifest, SubcProvider } from "../../src/index.js";

const owner = process.env.SUBC_MODULE_ID!;
const provider = await SubcProvider.connect({
  connectionFile: join(process.env.XDG_RUNTIME_DIR!, "subc-connection.json"),
  manifest: managementSurfaceManifest({ moduleId: owner, operations: ["end"] }),
  handler: async () => Buffer.from(JSON.stringify(await provider.scopeApply(3, [], [
    { scope_ref: "rig", scope_epoch: 1 },
  ]))),
});

const sync = await provider.scopeSync(1, [{
  scope_ref: "rig", scope_epoch: 1, kind: "head", workdir: process.env.SCOPE_WORKDIR!,
  carriers: [{ principal: { kind: "reserved", module_id: "ts-scope-carrier" } }],
}]);
const apply = await provider.scopeApply(2, [{
  scope_ref: "forbidden-attribution", scope_epoch: 1, kind: "head", attributes: { agent_id: "agent" },
}], []);
writeFileSync(process.env.SCOPE_RESULTS!, JSON.stringify({ sync, apply }));
await provider.closed;
process.exit(0);
