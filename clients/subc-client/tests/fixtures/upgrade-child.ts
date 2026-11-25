import { plugin } from "bun";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

// Provide the published native binding as `ck:registration`, so the runner
// loader can adopt an inherited descriptor without a Rust build.
const published = import.meta.resolve("subc-client-published");
const binary = new URL(`../native/subc-registration-${process.platform}-${process.arch}.node`, published);
const binding = createRequire(import.meta.url)(fileURLToPath(binary));
let takes = 0;
plugin({ name: "test-registration", setup(build) {
  build.onResolve({ filter: /^registration$/, namespace: "ck" }, ({ path }) => ({ path, namespace: "ck" }));
  build.onLoad({ filter: /^registration$/, namespace: "ck" }, () => ({ loader: "object", exports: {
    ...binding,
    takeRegistrationChannel() {
      if (++takes !== 1) throw new Error("registration channel was adopted twice");
      return binding.takeRegistrationChannel();
    },
  } }));
} });

const { SubcProvider, managementSurfaceManifest } = await import("../../src/index.js");
const { registrationOwner } = await import("../../src/registration-channel.js");
const manifest = managementSurfaceManifest({ moduleId: "upgrade-worker", operations: [] });
manifest.registration_channel = true;
const provider = await SubcProvider.connect({ connectionFile: process.argv[3]!, manifest, handler() {}, liveUpgrade: true,
  async resumeScopeSync() { return { generation: 3, scopes: [] }; },
  async onReconnected(handle, outcome) {
    if (!outcome?.ok || outcome.reply.generation !== 3) throw new Error("scope sync outcome was lost");
    const claim = await registrationOwner().broker!.mint({ purpose: "helper" });
    if (claim !== "c".repeat(22)) throw new Error("fresh claim was not minted");
    await handle.scopeSync(4, []);
    console.log("RECONNECTED");
  },
});
if (process.argv[2] === "upgrade-prefix") {
  const prefix = await SubcProvider.connect({ connectionFile: process.argv[3]!,
    manifest: { ...manifest, module_id: "upgrade-worker:prefix" }, handler() {}, liveUpgrade: true,
  });
  await prefix.closed;
}
await provider.closed;
console.log("CLOSED");
