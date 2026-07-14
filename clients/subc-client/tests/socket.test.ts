import { expect, test } from "bun:test";
import { createServer, type AddressInfo } from "node:net";

import { buildFrame, encodeFrame, FrameType, Priority, buildFlags, DecodeError } from "../src/envelope.js";
import { SocketTimeoutError, SubcSocket, toWriteBuffer } from "../src/socket.js";

test("outbound write buffer preserves the exact slice without copying", () => {
  const storage = new Uint8Array([99, 1, 2, 3, 88]);
  const bytes = storage.subarray(1, 4);

  const writeBuffer = toWriteBuffer(bytes);

  expect([...writeBuffer]).toEqual([1, 2, 3]);
  expect(writeBuffer.buffer).toBe(bytes.buffer);
});

test("prefix-first reader rejects a stale 17-byte v1 header without waiting for byte 18", async () => {
  const server = createServer((socket) => {
    const staleHeader = new Uint8Array(17);
    staleHeader[4] = 1;
    socket.write(staleHeader);
    // Keep the peer open: a fixed 21-byte read would hang here.
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  try {
    const result = await Promise.race([
      socket.readFrame(Number.POSITIVE_INFINITY, Date.now() + 1_000).then(
        () => "resolved",
        (error: unknown) => error,
      ),
      new Promise<string>((resolve) => setTimeout(() => resolve("hung"), 100)),
    ]);
    expect(result).toBeInstanceOf(DecodeError);
    expect((result as Error).message).toBe("unsupported envelope version 1");
  } finally {
    socket.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

// A background frame loop waits for the next frame's header with an infinite
// deadline. When a frame finally arrives after the connection has been quiet
// for LONGER than the body-read timeout, the body budget must start at header
// arrival — not when readFrame was called — or the body read instant-rejects a
// perfectly good frame ("timed out waiting for N bytes"). This is the idle >
// body-timeout regression that broke every subc-client 0.4.0 consumer after a
// >30s quiet stretch. Here the miniature is: body timeout 60ms, header arrives
// at 200ms.
async function frameAfterIdle(idleMs: number): Promise<{ port: number; close: () => Promise<void> }> {
  const body = new TextEncoder().encode("hello-after-a-long-idle");
  const frame = buildFrame(FrameType.Response, buildFlags(false, Priority.Passive, true), 7, 1, 42n, body);
  const wire = encodeFrame(frame);
  const server = createServer((socket) => {
    setTimeout(() => socket.write(wire), idleMs);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    port: (server.address() as AddressInfo).port,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

test("afterHeaderMs re-anchors the body budget to header arrival: a frame after a long idle still reads", async () => {
  const { port, close } = await frameAfterIdle(200);
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  try {
    // Body timeout 60ms, but the header does not arrive for 200ms. With the
    // afterHeaderMs form the 60ms clock starts at header arrival, so the body
    // (sent in the same write) reads well within budget.
    const frame = await socket.readFrame(Number.POSITIVE_INFINITY, { afterHeaderMs: 60 });
    expect(new TextDecoder().decode(frame.body)).toBe("hello-after-a-long-idle");
    expect(frame.header.epoch).toBe(1);
  } finally {
    socket.close();
    await close();
  }
});

test("absolute body deadline still enforces a total budget (handshake semantics preserved, and the fix is non-vacuous)", async () => {
  const { port, close } = await frameAfterIdle(200);
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  try {
    // The handshake form passes an absolute deadline shared with the header
    // read. A frame arriving 200ms in against a 60ms-from-now deadline must
    // reject — proving (a) absolute mode keeps its total-budget guard and
    // (b) the afterHeaderMs test above is non-vacuous (this is the pre-fix
    // behavior the loop was wrongly getting).
    const result = await socket.readFrame(Number.POSITIVE_INFINITY, Date.now() + 60).then(
      () => "resolved",
      (error: unknown) => error,
    );
    expect(result).toBeInstanceOf(SocketTimeoutError);
  } finally {
    socket.close();
    await close();
  }
});
