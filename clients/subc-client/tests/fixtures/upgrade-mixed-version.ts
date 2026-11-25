import { SubcProvider as ReleasedProvider } from "subc-client-published";
import { managementSurfaceManifest, SubcProvider } from "../../src/index.js";

const connectionFile = process.argv[2]!;
const manifest = managementSurfaceManifest({ moduleId: "upgrade-worker", operations: [] });
const options = { connectionFile, manifest, handler() {} };

const released = await ReleasedProvider.connect(options);
await released.closed;
for (let i = 0; i < Number(process.argv[3]); i++) {
  const provider = await SubcProvider.connect({ ...options, liveUpgrade: true, launchNonce: "fixture-nonce", reconnectOnDrop: false });
  await provider.closed;
  console.log("CLOSED");
}
