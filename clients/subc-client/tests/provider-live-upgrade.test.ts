import { expect, spyOn, test } from "bun:test";
import { fileURLToPath } from "node:url";
import {
  FrameType, LIVE_UPGRADE_CAPABILITY, SocketClosedError, StaleRouteHandleError, SubcProvider,
  type ConfigView, type ModuleConfigOptions, type RouteHandle, type SubcProviderConnectOptions,
} from "../src/index.js";
import { registrationOwner } from "../src/registration-channel.js";
import { rejectionOf } from "./settle.js";
import { authenticate, deferred, frame, json, notice, readHello, UpgradePeer } from "./upgrade-peer.js";

const token = "A".repeat(43);
const rotated = "E".repeat(43);
const unixTest = process.platform === "darwin" || process.platform === "linux" ? test : test.skip;

async function connect(peer: UpgradePeer, options: Partial<SubcProviderConnectOptions> = {}) {
  const opening = SubcProvider.connect({ connectionFile: peer.connectionFile, manifest: peer.manifest,
    liveUpgrade: true, launchNonce: "fixture-nonce", reconnectOnDrop: false, handler() {}, ...options,
  });
  const socket = await peer.nextTcp();
  const hello = await readHello(socket);
  await peer.write(socket, peer.ack(1n, { resume_token: token }));
  return { provider: await opening, socket, hello };
}

function bind(corr: bigint, channel = 7) {
  return frame(FrameType.Request, corr, { op: "route.bind", route_channel: channel, epoch: 1,
    target: { kind: "management_surface", module_id: "upgrade-worker" },
    identity: { project_root: "/", harness: "test", session: "test" },
  });
}

unixTest("resumes after missing socket, stale auth and legacy refusal, with scope sync first and configuration alongside the hook", async () => {
  const peer = await UpgradePeer.start();
  const bound = deferred<RouteHandle>();
  const handler = deferred<AbortSignal>();
  const finishHandler = deferred<void>();
  const handlerFinished = deferred<void>();
  const finalHandler = deferred<AbortSignal>();
  const finishFinalHandler = deferred<void>();
  const oldApplyStarted = deferred<void>();
  const finishOldApply = deferred<void>();
  const blockedBind = deferred<void>();
  const finishBind = deferred<void>();
  const scopeStarted = deferred<SubcProvider>();
  const sendScope = deferred<void>();
  const applied = deferred<void>();
  const reconnected = deferred<void>();
  const configErrors: unknown[] = [];
  let attempts = 0;
  let retries = 0;
  let now = 0;
  const clock = spyOn(performance, "now").mockImplementation(() => now);
  let oldApplyCount = 0;
  const view: ConfigView = { generation: 1, status: "active", effective: {}, layers: [], sources: {} };
  const config: ModuleConfigOptions = {
    declaration: { schema: {}, user_file: "upgrade.json", project_dirs: [] },
    review() { throw new Error("unexpected config.review"); },
    readFiles() { return { ...view, generation: 0, status: "unstamped" }; },
    async apply(configView) {
      if (configView.generation === 1) applied.resolve();
      else if (++oldApplyCount === 2) {
        oldApplyStarted.resolve();
        await finishOldApply.promise;
      }
      return { result: "ack" };
    },
    onError(error) { configErrors.push(error); },
  };
  let provider: SubcProvider | undefined;
  try {
    const old = await connect(peer, {
      config,
      async handler(_handle, data, context) {
        if (Buffer.from(data).toString() === '"until final stop"') {
          finalHandler.resolve(context.signal);
          await finishFinalHandler.promise;
          return;
        }
        handler.resolve(context.signal);
        await finishHandler.promise;
        await context.emit(Buffer.from("stale stream piece"));
        handlerFinished.resolve();
        return Buffer.from("stale terminal reply");
      },
      async onBind(request) {
        if (request.handle.channel === 8) { blockedBind.resolve(); await finishBind.promise; }
        return true;
      },
      onBound(handle) { bound.resolve(handle); },
      onConnectionState(event) {
        if (event.state === "reconnecting") attempts++;
      },
      async sleep(ms) {
        now += ms;
        // Backoff means the preceding dial has failed. Publish the next
        // discovery record before the provider reads it, not from a callback
        // that can race the read on a busy runner.
        if (++retries === 1) peer.publish(peer.unixPath, true);
        else if (retries === 2) peer.publish();
      },
      async resumeScopeSync(handle) {
        if (handle.currentEpoch() > 2) return undefined;
        scopeStarted.resolve(handle);
        await sendScope.promise;
        return { generation: 9, scopes: [] };
      },
      async onReconnected(handle, outcome) {
        if (handle.currentEpoch() > 2) return;
        expect(handle).toBe(provider!);
        expect(outcome).toMatchObject({ ok: false, error: { code: "scope_epoch_in_use" } });
        await applied.promise;
        expect(await handle.scopeSync(10, [])).toEqual({ generation: 10, results: [], ended: [] });
        reconnected.resolve();
      },
    });
    provider = old.provider;
    expect(old.hello).toMatchObject({ module_capabilities: [LIVE_UPGRADE_CAPABILITY], launch_nonce: "fixture-nonce" });
    expect(old.hello.resume_token).toBeUndefined();
    await peer.write(old.socket, bind(20n));
    expect((await peer.read(old.socket)).header.ty).toBe(FrameType.Response);
    const route = await bound.promise;
    const oldRefresh = provider.config!.refresh();
    await oldApplyStarted.promise;
    await peer.write(old.socket, frame(FrameType.Request, 21n, { old: true }, route.channel, route.epoch),
      frame(FrameType.Request, 23n, "until final stop", route.channel, route.epoch));
    const dataSignal = await handler.promise;
    const finalSignal = await finalHandler.promise;
    const written = provider.request(route, Buffer.from("written"));
    const writtenFrame = await peer.read(old.socket);
    const abandoned = rejectionOf(provider.request(route, Buffer.from("abandoned")));
    expect((await peer.read(old.socket)).header.ty).toBe(FrameType.Request);
    await peer.write(old.socket, bind(22n, 8));
    await blockedBind.promise;
    await peer.write(old.socket, notice(), frame(FrameType.Ping, 90n));
    expect((await peer.read(old.socket)).header).toMatchObject({ ty: FrameType.Pong, corr: 90n });
    for (const fenced of [provider.request(route, Buffer.from("fenced")), provider.push(route, Buffer.from("fenced")),
      provider.scopeSync(1, []), provider.scopeApply(1, [], []), provider.config!.refresh()]) {
      expect(await rejectionOf(fenced)).toMatchObject({ kind: "not_sent", code: "daemon_upgrade" });
    }
    peer.publish(`${peer.unixPath}.missing`);
    await peer.write(old.socket, frame(FrameType.Response, writtenFrame.header.corr, "old outcome", route.channel, route.epoch),
      frame(FrameType.Goodbye));
    expect(Buffer.from(await written).toString()).toBe('"old outcome"');
    expect(await abandoned).toMatchObject({ kind: "outcome_unknown", code: "connection_dropped" });

    const whileResuming = <T>(work: Promise<T>) => Promise.race([work, provider!.closed.then(() => {
      throw new Error("provider closed before the expected resume connection");
    })]);
    const stale = await whileResuming(peer.unix.accept());
    expect(await rejectionOf(authenticate(stale))).toBeInstanceOf(SocketClosedError);
    const legacy = await whileResuming(peer.nextUnix());
    expect(await readHello(legacy)).toMatchObject({ resume_token: token, module_capabilities: [LIVE_UPGRADE_CAPABILITY] });
    await peer.write(legacy, frame(FrameType.Error, 1n, { code: "local_endpoint_clients_only", message: "legacy listener" }));
    const resumed = await whileResuming(peer.nextUnix());
    const hello = await readHello(resumed);
    expect(hello).toMatchObject({ resume_token: token, module_capabilities: [LIVE_UPGRADE_CAPABILITY], manifest: old.hello.manifest });
    expect(hello.launch_nonce).toBeUndefined();
    expect(attempts).toBe(4);
    expect(retries).toBe(3);
    expect(peer.tcp.count).toBe(1);
    await peer.write(resumed, peer.ack(1n, { resume_token: rotated, storage: { path: "new storage" }, machine_id: "a".repeat(32),
      subc_ops: ["scope.sync", "claim.mint"], subc_capabilities: ["config.gate"] }), frame(FrameType.Ping, 91n));
    expect(await scopeStarted.promise).toBe(provider);
    expect(provider.currentEpoch()).toBe(2);
    expect(provider.storage).toEqual({ path: "new storage" });
    expect(provider.machineId).toBe("a".repeat(32));
    expect(provider.subcCapabilities).toEqual(["config.gate"]);
    expect(await rejectionOf(provider.scopeSync(1, []))).toMatchObject({ kind: "not_sent", code: "daemon_upgrade" });
    expect(await rejectionOf(registrationOwner().broker!.mint({ purpose: "helper" })))
      .toMatchObject({ kind: "not_sent", code: "daemon_upgrade" });
    sendScope.resolve();
    const first = await peer.read(resumed);
    expect(json(first)).toEqual({ op: "scope.sync", generation: 9, scopes: [] });
    await peer.write(resumed, frame(FrameType.Error, first.header.corr, { code: "scope_epoch_in_use", message: "fixture refusal" }));
    const next = [await peer.read(resumed), await peer.read(resumed)];
    expect(next.find((msg) => msg.header.ty === FrameType.Pong)?.header.corr).toBe(91n);
    const declare = next.find((msg) => msg.header.ty === FrameType.Request)!;
    expect(json(declare).op).toBe("config.declare");
    await peer.write(resumed, frame(FrameType.Response, declare.header.corr, { op: "config.declare" }));
    const get = await peer.read(resumed);
    expect(json(get).op).toBe("config.get");
    await peer.write(resumed, frame(FrameType.Response, get.header.corr, { op: "config.get", active: view }));
    const sync = await peer.read(resumed);
    expect(json(sync)).toEqual({ op: "scope.sync", generation: 10, scopes: [] });
    await peer.write(resumed, frame(FrameType.Response, sync.header.corr, { op: "scope.sync", generation: 10, results: [] }));
    await reconnected.promise;
    finishOldApply.resolve();
    await oldRefresh;
    expect(configErrors).toEqual([]);
    expect(await rejectionOf(provider.request(route, Buffer.from("stale")))).toBeInstanceOf(StaleRouteHandleError);
    expect(dataSignal.aborted).toBe(false);
    expect(finalSignal.aborted).toBe(false);
    finishHandler.resolve();
    finishBind.resolve();
    await handlerFinished.promise;
    await peer.write(resumed, frame(FrameType.Ping, 92n));
    expect((await peer.read(resumed)).header).toMatchObject({ ty: FrameType.Pong, corr: 92n });

    // A later upgrade must authenticate with the new token, even when the old socket ends by EOF.
    await peer.write(resumed, notice());
    await peer.write(resumed, frame(FrameType.Ping, 93n));
    expect((await peer.read(resumed)).header.ty).toBe(FrameType.Pong);
    resumed.close();
    const nextImage = await whileResuming(peer.nextUnix());
    expect((await readHello(nextImage)).resume_token).toBe(rotated);
    await peer.write(nextImage, peer.ack(1n), frame(FrameType.Ping, 94n));
    expect((await peer.read(nextImage)).header).toMatchObject({ ty: FrameType.Pong, corr: 94n });
    await peer.write(nextImage, frame(FrameType.Goodbye));
    await provider.closed;
    expect(finalSignal.aborted).toBe(true);
    expect(peer.tcp.count).toBe(1);
  } finally {
    clock.mockRestore();
    finishHandler.resolve(); finishBind.resolve(); sendScope.resolve(); applied.resolve();
    finishOldApply.resolve(); finishFinalHandler.resolve();
    await provider?.close();
    await peer.stop();
  }
});

for (const code of ["resume_refused", "unknown_future_refusal", "duplicate_module_id"]) {
  unixTest(`resume treats ${code} as terminal without retry or hook`, async () => {
    const peer = await UpgradePeer.start();
    let attemptCount = 0;
    let hooks = 0;
    let provider: SubcProvider | undefined;
    try {
      const old = await connect(peer, {
        onConnectionState: (event) => { if (event.state === "reconnecting") attemptCount++; },
        onReconnected: () => { hooks++; },
      });
      provider = old.provider;
      await peer.write(old.socket, notice(), frame(FrameType.Goodbye));
      const local = await peer.nextUnix();
      expect((await readHello(local)).resume_token).toBe(token);
      await peer.write(local, frame(FrameType.Error, 1n, { code }));
      await provider.closed;
      expect(attemptCount).toBe(1);
      expect(peer.tcp.count).toBe(1);
      expect(hooks).toBe(0);
    } finally { await provider?.close(); await peer.stop(); }
  });
}

unixTest("the announced deadline closes a connection still waiting for HELLO_ACK", async () => {
  const peer = await UpgradePeer.start();
  let provider: SubcProvider | undefined;
  try {
    const old = await connect(peer, { handshakeTimeoutMs: 9000 });
    provider = old.provider;
    await peer.write(old.socket, notice(Date.now() + 500), frame(FrameType.Goodbye));
    const local = await peer.nextUnix();
    await readHello(local);
    expect(await rejectionOf(peer.read(local))).toBeInstanceOf(SocketClosedError);
    await provider.closed;
    expect(peer.tcp.count).toBe(1);
  } finally { await provider?.close(); await peer.stop(); }
});

test("without opt-in or launch identity HELLO omits upgrade capability and GOODBYE ends serving", async () => {
  const peer = await UpgradePeer.start(false);
  try {
    for (const options of [{ liveUpgrade: false }, { launchNonce: "" }]) {
      const { provider, socket, hello } = await connect(peer, options);
      expect(hello.module_capabilities).toBeUndefined();
      await peer.write(socket, notice(), frame(FrameType.Goodbye));
      await provider.closed;
    }
    expect(peer.tcp.count).toBe(2);
  } finally { await peer.stop(); }
});

for (const refusal of [false, true]) {
  unixTest(`inherited registration ${refusal ? "exits as EOF on resume_refused" : "resumes once and keeps serving"}`, async () => {
    const peer = await UpgradePeer.start();
    const launcher = fileURLToPath(new URL("./fixtures/registration-launcher.py", import.meta.url));
    const entry = fileURLToPath(new URL("./fixtures/upgrade-child.ts", import.meta.url));
    const address = peer.tcp.server.address() as { port: number };
    const env = { ...process.env, SUBC_MODULE_ID: "upgrade-worker" };
    const child = Bun.spawn(["python3", launcher, String(address.port), process.execPath, "--conditions=ck-script-runner",
      entry, refusal ? "upgrade" : "upgrade-prefix", peer.connectionFile], { env, stdout: "pipe", stderr: "pipe" });
    const text = Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
    try {
      const inherited = await peer.nextTcp();
      const hello = await readHello(inherited);
      expect(hello.module_capabilities).toEqual([LIVE_UPGRADE_CAPABILITY]);
      expect(hello.launch_nonce).toBeUndefined();
      await peer.write(inherited, peer.ack(1n, { resume_token: token }));
      if (!refusal) {
        const mint = await peer.read(inherited);
        expect(json(mint)).toEqual({ op: "claim.mint", purpose: "prefix", module_id: "upgrade-worker:prefix" });
        const claim = "p".repeat(22);
        await peer.write(inherited, frame(FrameType.Response, mint.header.corr, { op: "claim.mint", claim, expires_at_ms: Date.now() + 30000 }));
        const prefix = await peer.nextTcp();
        const present = await peer.read(prefix);
        expect(json(present)).toEqual({ op: "claim.present", claim });
        await peer.write(prefix, frame(FrameType.Response, present.header.corr, { op: "claim.present" }));
        expect((await readHello(prefix)).module_capabilities).toBeUndefined();
        await peer.write(prefix, peer.ack(1n), frame(FrameType.Goodbye));
      }
      await peer.write(inherited, notice(), frame(FrameType.Goodbye));
      const local = await peer.nextUnix();
      expect((await readHello(local)).resume_token).toBe(token);
      if (refusal) {
        await peer.write(local, frame(FrameType.Error, 1n, { code: "resume_refused" }));
      } else {
        await peer.write(local, peer.ack(1n, { resume_token: rotated }));
        const sync = await peer.read(local);
        expect(json(sync)).toEqual({ op: "scope.sync", generation: 3, scopes: [] });
        await peer.write(local, frame(FrameType.Response, sync.header.corr, { op: "scope.sync", generation: 3, results: [] }));
        const mint = await peer.read(local);
        expect(json(mint)).toEqual({ op: "claim.mint", purpose: "helper" });
        await peer.write(local, frame(FrameType.Response, mint.header.corr, { op: "claim.mint",
          claim: "c".repeat(22), expires_at_ms: Date.now() + 30000 }));
        const finished = await peer.read(local);
        expect(json(finished)).toEqual({ op: "scope.sync", generation: 4, scopes: [] });
        await peer.write(local, frame(FrameType.Response, finished.header.corr, { op: "scope.sync", generation: 4, results: [] }));
        await peer.write(local, frame(FrameType.Goodbye));
      }
      const [out, err] = await text;
      expect(err).toBe("");
      expect(await child.exited).toBe(refusal ? 1 : 0);
      expect(out.trim()).toBe(refusal ? "CLOSED" : "RECONNECTED\nCLOSED");
      expect(peer.tcp.count).toBe(refusal ? 1 : 2);
    } finally {
      await peer.stop();
      if (child.exitCode === null) child.kill();
      await child.exited;
    }
  }, 12_000);
}
