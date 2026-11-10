import { spawn } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { SubcClient, SubcProvider, managementSurfaceManifest, prepareHelperClaim } from "../../src/index";
import { registrationOwner } from "../../src/registration-channel";

const mode = process.argv[2]!;
const connectionFile = process.argv[3]!;
const assert = (condition: unknown) => { if (!condition) throw new Error("registration fixture assertion failed"); };
if (mode === "direct" || mode === "helper-child") {
  assert(!registrationOwner().supervised);
  const client = await SubcClient.connect({connectionFile});
  await client.catalogList();
  console.log(mode === "direct" ? "DIRECT_OK" : "HELPER_OK");
  await client.close();
  process.exit(0);
}
const manifest = managementSurfaceManifest({moduleId: "test-channel", operations: []});
manifest.registration_channel = true;
const provider = await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array(), reconnectOnDrop: true});
assert(registrationOwner().supervised);
if (mode === "eof") {
  setInterval(() => {}, 1000);
  await provider.closed;
  console.log("CLOSED_HOOK");
} else if (mode === "mint-refused" || mode === "present-refused") {
  let refused = false;
  try { await SubcClient.connect({connectionFile}); } catch { refused = true; }
  assert(refused);
  console.log("REFUSED_OK");
  process.exit(0);
} else {
  const ordinary = spawn(process.execPath, [fileURLToPath(import.meta.url), "direct", connectionFile], {
    env: {...process.env}, stdio: ["ignore", "pipe", "pipe"],
  });
  let ordinaryOutput = "";
  ordinary.stdout!.on("data", bytes => { ordinaryOutput += bytes.toString(); });
  const [ordinaryStatus] = await once(ordinary, "exit");
  assert(ordinaryStatus === 0 && ordinaryOutput.includes("DIRECT_OK"));
  const clients = await Promise.all([SubcClient.connect({connectionFile}), SubcClient.connect({connectionFile})]);
  for (const client of clients) await client.catalogList();
  let refused = false;
  try { await clients[0]!.routeOpen({kind: "management_surface", module_id: "target"}, {project_root: "/", harness: "test", session: "test"}, {consumerIdentity: null}); }
  catch (error) { refused = (error as {code?: string}).code === "claim_identity_conflict"; }
  assert(refused);
  let second = false;
  try { await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array()}); }
  catch (error) { second = (error as {code?: string}).code === "registration_already_taken"; }
  assert(second);
  const handoff = prepareHelperClaim();
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "helper-child", connectionFile], {
    env: {...process.env, ...handoff.environment},
    stdio: handoff.childFd === null ? ["ignore", "pipe", "pipe"] : ["ignore", "pipe", "pipe", handoff.childFd],
  });
  const exit = once(child, "exit");
  let output = "";
  child.stdout!.on("data", bytes => { output += bytes.toString(); });
  child.once("error", () => handoff.abort());
  await once(child, "spawn");
  await handoff.complete(child.pid!);
  const [status] = await exit;
  assert(status === 0 && output.includes("HELPER_OK"));
  console.log("CHANNEL_OK");
  process.exit(0);
}
