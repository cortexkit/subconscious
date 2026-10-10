import { expect, test } from "bun:test";
import { createServer, type AddressInfo, type Socket } from "node:net";

import { buildFrame, encodeFrame, FrameType, Priority, buildFlags, DecodeError, HEADER_LEN } from "../src/envelope.js";
import { SocketTimeoutError, SubcSocket, toWriteBuffer } from "../src/socket.js";

test("outbound write buffer preserves the exact slice without copying", () => {
  const storage = new Uint8Array([99, 1, 2, 3, 88]);
  const bytes = storage.subarray(1, 4);

  const writeBuffer = toWriteBuffer(bytes);

  expect([...writeBuffer]).toEqual([1, 2, 3]);
  expect(writeBuffer.buffer).toBe(bytes.buffer);
});

test("public write owns queued bytes while the caller reuses its source buffer", async () => {
  const queued: Buffer[] = [];
  let completeWrite!: () => void;
  const socket = Object.create(SubcSocket.prototype) as SubcSocket;
  const internals = socket as unknown as {
    sock: {
      write(bytes: Buffer, callback: (error?: Error | null) => void): boolean;
    };
    closedErr: Error | null;
  };
  internals.closedErr = null;
  internals.sock = {
    write(bytes, callback) {
      queued.push(bytes);
      completeWrite = () => callback();
      return false;
    },
  };
  const source = new Uint8Array([1, 2, 3]);

  const completed = socket.write(source, Date.now() + 1_000);
  source.fill(9);
  const observedAfterMutation = [...queued[0]!];
  completeWrite();
  await completed;

  expect(observedAfterMutation).toEqual([1, 2, 3]);
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

test("readFrame succeeds when event loop is blocked longer than body budget while peer has already written full body", async () => {
  const body = new TextEncoder().encode("payload-sitting-in-kernel-buffer");
  const frame = buildFrame(FrameType.Response, buildFlags(false, Priority.Passive, true), 1, 1, 10n, body);
  const wire = encodeFrame(frame);
  const headerBytes = wire.subarray(0, HEADER_LEN);
  const bodyBytes = wire.subarray(HEADER_LEN);

  let peerSock!: Socket;
  const server = createServer((s) => {
    peerSock = s;
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  await new Promise<void>((r) => {
    if (peerSock) r();
    else server.once("connection", () => r());
  });

  try {
    peerSock.write(headerBytes);
    let headerSeen = false;
    const readPromise = socket.readFrame(Number.POSITIVE_INFINITY, { afterHeaderMs: 40 }, () => {
      headerSeen = true;
      // In onHeader, the header has arrived and readExact for the body is about
      // to be invoked. We queue a microtask so readExact registers its waiter
      // and arms its 40ms timer before the peer writes the body to the kernel
      // buffer and the event loop is synchronously blocked.
      queueMicrotask(() => {
        peerSock.write(bodyBytes);
        const start = Date.now();
        while (Date.now() - start < 80) {}
      });
    });

    const result = await readPromise;
    expect(headerSeen).toBe(true);
    expect(new TextDecoder().decode(result.body)).toBe("payload-sitting-in-kernel-buffer");
  } finally {
    socket.close();
    peerSock?.destroy();
    await new Promise<void>((r) => server.close(() => r()));
  }
});

test("body that trickles in slower than budget overall with gaps shorter than budget succeeds", async () => {
  const body = new TextEncoder().encode("trickling-body-chunks-slow-stream");
  const frame = buildFrame(FrameType.Response, buildFlags(false, Priority.Passive, true), 1, 1, 10n, body);
  const wire = encodeFrame(frame);
  const headerBytes = wire.subarray(0, HEADER_LEN);
  const bodyBytes = wire.subarray(HEADER_LEN);

  let peerSock!: Socket;
  const server = createServer((s) => {
    peerSock = s;
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  await new Promise<void>((r) => {
    if (peerSock) r();
    else server.once("connection", () => r());
  });

  try {
    peerSock.write(headerBytes);
    // Budget is 40ms. Body is split into 3 chunks sent at 0ms, 25ms, and 50ms.
    // Total duration is 50ms > 40ms budget, but each gap (25ms) is shorter than 40ms.
    const part1 = bodyBytes.subarray(0, 10);
    const part2 = bodyBytes.subarray(10, 20);
    const part3 = bodyBytes.subarray(20);

    const readPromise = socket.readFrame(Number.POSITIVE_INFINITY, { afterHeaderMs: 40 });

    setTimeout(() => peerSock.write(part1), 0);
    setTimeout(() => peerSock.write(part2), 25);
    setTimeout(() => peerSock.write(part3), 50);

    const result = await readPromise;
    expect(new TextDecoder().decode(result.body)).toBe("trickling-body-chunks-slow-stream");
  } finally {
    socket.close();
    peerSock?.destroy();
    await new Promise<void>((r) => server.close(() => r()));
  }
});

test("body that stops arriving fails with SocketTimeoutError after budget", async () => {
  const body = new TextEncoder().encode("abandoned-body-incomplete");
  const frame = buildFrame(FrameType.Response, buildFlags(false, Priority.Passive, true), 1, 1, 10n, body);
  const wire = encodeFrame(frame);
  const headerBytes = wire.subarray(0, HEADER_LEN);
  const bodyBytes = wire.subarray(HEADER_LEN);

  let peerSock!: Socket;
  const server = createServer((s) => {
    peerSock = s;
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  await new Promise<void>((r) => {
    if (peerSock) r();
    else server.once("connection", () => r());
  });

  try {
    peerSock.write(headerBytes);
    // Send only the first 5 bytes of the body, then abandon the stream.
    const partialBody = bodyBytes.subarray(0, 5);

    const readPromise = socket.readFrame(Number.POSITIVE_INFINITY, { afterHeaderMs: 40 });
    peerSock.write(partialBody);

    const err = await Promise.race([
      readPromise.then(
        () => null,
        (e: unknown) => e,
      ),
      new Promise<string>((resolve) => setTimeout(() => resolve("timed-out-in-test"), 150)),
    ]);
    expect(err).toBeInstanceOf(SocketTimeoutError);
    expect((err as Error).message).toBe(`timed out waiting for ${body.length} bytes`);
  } finally {
    socket.close();
    peerSock?.destroy();
    await new Promise<void>((r) => server.close(() => r()));
  }
});

test("an absolute deadline is not extended by bytes that keep trickling in", async () => {
  const body = new TextEncoder().encode("a-body-sent-one-byte-at-a-time-forever");
  const frame = buildFrame(FrameType.Response, buildFlags(false, Priority.Passive, true), 1, 1, 10n, body);
  const wire = encodeFrame(frame);
  const headerBytes = wire.subarray(0, HEADER_LEN);
  const bodyBytes = wire.subarray(HEADER_LEN);

  let peerSock!: Socket;
  const server = createServer((s) => {
    peerSock = s;
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const socket = await SubcSocket.connect("127.0.0.1", port, Date.now() + 1_000);
  await new Promise<void>((r) => {
    if (peerSock) r();
    else server.once("connection", () => r());
  });

  // One byte every 10 ms keeps progress flowing well past the 60 ms absolute
  // deadline; the read must still fail at that deadline, as the handshake
  // relies on, instead of being extended by each byte.
  let sent = 0;
  const drip = setInterval(() => {
    if (sent < bodyBytes.length) peerSock.write(bodyBytes.subarray(sent, ++sent));
  }, 10);
  try {
    peerSock.write(headerBytes);
    const started = Date.now();
    const result = await socket.readFrame(Number.POSITIVE_INFINITY, Date.now() + 60).then(
      () => "resolved",
      (error: unknown) => error,
    );
    expect(result).toBeInstanceOf(SocketTimeoutError);
    expect(Date.now() - started).toBeLessThan(250);
  } finally {
    clearInterval(drip);
    socket.close();
    peerSock?.destroy();
    await new Promise<void>((r) => server.close(() => r()));
  }
});
