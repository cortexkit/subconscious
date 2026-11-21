import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { buildFlags, buildFrame, encodeFrame, FrameType, Priority, SubcSocket } from "../src/index.js";

const folder = join(import.meta.dir, "../../../vectors/config-settings-v1");
const projectVectors = readdirSync(folder).filter((name) => name.startsWith("project-") && name.endsWith(".json")).sort();
if (!projectVectors.length) throw new Error("missing project settings vectors");

for (const name of projectVectors) {
  test(`channel-0 JSON decodes ${name}`, async () => {
    const sample = JSON.parse(readFileSync(join(folder, name), "utf8"));
    const frames: [FrameType, object][] = [];
    if (sample.request) {
      frames.push([FrameType.Request, sample.request]);
      if (sample.reply) frames.push([sample.reply.code ? FrameType.Error : FrameType.Response, sample.reply]);
    } else {
      frames.push([sample.op === "config.watch" ? FrameType.StreamData : FrameType.Push, sample]);
    }

    const stream = new PassThrough();
    const socket = SubcSocket.adopt(stream);
    const flags = buildFlags(false, Priority.Passive, false);
    try {
      for (const [ty, body] of frames) {
        const corr = ty === FrameType.Push ? 0n : 1n;
        stream.write(encodeFrame(buildFrame(ty, flags, 0, 0, corr, Buffer.from(JSON.stringify(body), "utf8"))));
        const deadline = Date.now() + 1000;
        const frame = await socket.readFrame(deadline, deadline);
        expect(frame.header).toMatchObject({ ty, flags, channel: 0, epoch: 0, corr });
        expect(JSON.parse(Buffer.from(frame.body).toString("utf8"))).toEqual(body);
      }
    } finally { socket.close(); }
  });
}
