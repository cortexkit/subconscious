import { buildFrame, FrameType, type Frame } from "../src/envelope.js";

/** Scripted older daemons answer optional discovery without advertising new extensions. */
export function serverDescribeReply(frame: Frame, capabilities: string[] = []): Frame | undefined {
  if (frame.header.ty !== FrameType.Request || frame.header.channel !== 0) return undefined;
  const body = JSON.parse(new TextDecoder().decode(frame.body)) as { op?: string };
  if (body.op !== "server.describe") return undefined;
  return buildFrame(FrameType.Response, 0, 0, 0, frame.header.corr,
    new TextEncoder().encode(JSON.stringify({ op: "server.describe", capabilities })));
}
