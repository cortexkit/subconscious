import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import {
  buildFrame,
  decodeHeader,
  encodeFrame,
  FrameType,
  FROZEN_PREFIX_LEN,
  HEADER_LEN,
  MAX_FRAME_BODY_LEN,
  PROTOCOL_VERSION,
} from "../src/envelope";

// The Rust golden fixtures are canonical serializations of the wire shapes both
// languages speak. Their README has always said the TypeScript client consumes
// them as conformance vectors, so that a Rust shape change without a matching TS
// change fails a build -- but nothing here read them, so the drift they were
// meant to catch would have surfaced against a live daemon instead.
//
// Types do not close this gap: TypeScript interfaces erase at runtime, so a
// field the client declares and a field the daemon sends can disagree with
// nothing to notice. Only code that reads the bytes can hold the contract.
const GOLDEN_DIR = join(
  import.meta.dir,
  "..",
  "..",
  "..",
  "crates",
  "subc-protocol",
  "tests",
  "golden",
);

function loadGolden(name: string): Record<string, unknown> {
  const raw = readFileSync(join(GOLDEN_DIR, `${name}.json`), "utf8");
  return JSON.parse(raw) as Record<string, unknown>;
}

describe("Rust golden fixtures", () => {
  test("are reachable from the TypeScript package", () => {
    // A path that silently resolves to nothing would make every assertion below
    // vacuous, so the suite proves it found the real directory before using it.
    const fixtures = readdirSync(GOLDEN_DIR).filter((name) => name.endsWith(".json"));
    expect(fixtures.length).toBeGreaterThan(0);
    expect(fixtures).toContain("error_body.json");
  });

  test("error bodies carry the fields the client reads off a failed frame", () => {
    const body = loadGolden("error_body");
    // errorFromFrame reads exactly these two, so a rename on the Rust side turns
    // every daemon error into the "subc error" fallback with its cause dropped:
    // a silent degradation rather than a failure, which is why it needs a test.
    expect(typeof body.code).toBe("string");
    expect(typeof body.message).toBe("string");
  });

  test("route targets keep the discriminator the client switches on", () => {
    // RouteTarget is a tagged union on `kind`; the client builds these and the
    // daemon matches them, so the tag values are a contract rather than a detail.
    const targets = {
      tool_provider: loadGolden("route_target_tool_provider"),
      management_surface: loadGolden("route_target_management_surface"),
      internal_service: loadGolden("route_target_internal_service"),
    };
    for (const [expectedKind, target] of Object.entries(targets)) {
      expect(target.kind).toBe(expectedKind);
      expect(typeof target.module_id).toBe("string");
    }
    expect(typeof targets.internal_service.service_id).toBe("string");
  });

  test("bind identity keeps the snake_case field the client sends", () => {
    // The client serializes this object by hand, so a casing change in Rust
    // would be accepted by TypeScript's types and rejected by the daemon.
    const identity = loadGolden("bind_identity");
    expect(typeof identity.project_root).toBe("string");
    expect(typeof identity.harness).toBe("string");
    expect(typeof identity.session).toBe("string");
  });

  test("the transcribed protocol constants match the Rust originals", () => {
    // Every client transcribes these four values, and only three of them are
    // protected by anything. PROTOCOL_VERSION, HEADER_LEN and FROZEN_PREFIX_LEN
    // appear IN encoded bytes, so a drift changes this client's output and the
    // committed frame vectors catch it. MAX_FRAME_BODY_LEN is a THRESHOLD -- it
    // appears in no byte of any frame, so no byte-parity fixture can observe it.
    //
    // What stood here instead was this package's own test importing this
    // package's own constant: true by construction, and silent if Rust changed.
    // A cap drifting low refuses frames the daemon considers legal; drifting
    // high accepts an allocation the daemon refuses. Both surface on a live wire
    // rather than in a build.
    const constants = loadGolden("protocol_constants");
    expect(constants.protocol_version).toBe(PROTOCOL_VERSION);
    expect(constants.header_len).toBe(HEADER_LEN);
    expect(constants.frozen_prefix_len).toBe(FROZEN_PREFIX_LEN);
    expect(constants.max_frame_body_len).toBe(MAX_FRAME_BODY_LEN);
  });

  test("a route bind command survives a real frame round trip", () => {
    // Carrying a fixture through the shipped encoder and decoder proves the two
    // layers agree on more than this test does: a body that encodes and decodes
    // back to identical bytes is one the daemon and client can actually exchange.
    const bind = loadGolden("module_control_request_route_bind");
    expect(bind.op).toBe("route.bind");

    const body = new Uint8Array(Buffer.from(JSON.stringify(bind), "utf8"));
    const wire = encodeFrame(buildFrame(FrameType.Request, 0, 0, 0, 7n, body));
    const header = decodeHeader(wire);
    expect(header.ty).toBe(FrameType.Request);
    expect(header.corr).toBe(7n);
    expect(header.len).toBe(body.length);

    const decodedBody = wire.subarray(wire.length - body.length);
    expect(JSON.parse(Buffer.from(decodedBody).toString("utf8"))).toEqual(bind);
  });
});
