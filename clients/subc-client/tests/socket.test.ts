import { expect, test } from "bun:test";
import { createServer, type AddressInfo } from "node:net";

import { DecodeError } from "../src/envelope.js";
import { SubcSocket } from "../src/socket.js";

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
