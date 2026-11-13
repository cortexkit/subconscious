import { appendFileSync, existsSync, renameSync, writeFileSync } from "node:fs";
import { SubcClient, SubcProvider, managementSurfaceManifest } from "../../../../clients/subc-client/src/index.ts";
import { launchNonceAsync } from "../../../../clients/subc-client/src/launch-nonce.ts";

const root = process.env.CHANNEL_MODULE_ROOT!;
const moduleId = process.env.SUBC_MODULE_ID!;
const connectionFile = `${root}/runtime/subc-connection.json`;
const nonce = await launchNonceAsync();
if (nonce) writeFileSync(`${root}/caller.nonce`, nonce.value);
const event = (value: object) => appendFileSync(`${root}/${moduleId}.events`, `${JSON.stringify(value)}\n`);
const provider = await SubcProvider.connect({
  connectionFile,
  manifest: managementSurfaceManifest({ moduleId, operations: ["read"] }),
  handler: async (_ctx, body) => body,
  config: {
    declaration: {
      user_file: `${root}/config/cortexkit/${moduleId}.jsonc`, project_dirs: [],
      schema: { $schema: "https://json-schema.org/draft/2020-12/schema", type: "object", properties: {}, unevaluatedProperties: false },
    },
    readFiles: () => ({ generation: 0, status: "unstamped", effective: {}, sources: {}, layers: [] }),
    review: () => { event({ event: "review" }); return { op: "config.review", refusals: [], fields: [] }; },
    apply: () => ({ result: "ack" }),
    onState: (state) => event({ event: "config", state: state.state }),
  },
});
while (!existsSync(`${root}/go`)) await new Promise((resolve) => setTimeout(resolve, 20));
const sync = await provider.scopeSync(1, []);
const apply = await provider.scopeApply(2, [], []);
// Connecting a consumer in the supervised process mints and presents a self claim.
const client = await SubcClient.connect({ connectionFile });
const route = await client.routeOpen({ kind: "tool_provider", module_id: "target" }, { harness: "channel-fixture", session: "session", project_root: root });
const reply = await client.request(route, { message: "child-bound roundtrip" }, { timeoutMs: 5000 });
writeFileSync(`${root}/result.staging`, JSON.stringify({ sync: { Ok: sync.generation }, apply: { Ok: apply.generation }, reply: { Ok: reply } }));
renameSync(`${root}/result.staging`, `${root}/result.json`);
await provider.closed;
