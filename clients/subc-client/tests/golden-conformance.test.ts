import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import type { CatalogEntry } from "../src/client";
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

const CONTROL_GOLDEN_DIR = join(
  import.meta.dir,
  "..",
  "..",
  "..",
  "crates",
  "subc-control",
  "tests",
  "golden",
);

function loadGolden(name: string): Record<string, unknown> {
  const raw = readFileSync(join(GOLDEN_DIR, `${name}.json`), "utf8");
  return JSON.parse(raw) as Record<string, unknown>;
}

function loadControlGolden(name: string): Record<string, unknown> {
  return JSON.parse(
    readFileSync(join(CONTROL_GOLDEN_DIR, `${name}.json`), "utf8"),
  ) as Record<string, unknown>;
}

describe("Rust golden fixtures", () => {
  test("route.closed fixtures carry reachable terminal verdicts", () => {
    const fixtures = {
      drained: loadControlGolden("client_control_push_route_closed_drained"),
      abandoned: loadControlGolden("client_control_push_route_closed_abandoned"),
      disable: loadControlGolden("client_control_push_route_closed_disable"),
      crash: loadControlGolden("client_control_push_route_closed_crash"),
      crashTerminal: loadControlGolden(
        "client_control_push_route_closed_crash_terminal",
      ),
    };

    for (const fixture of Object.values(fixtures)) {
      expect(typeof fixture.terminal).toBe("boolean");
    }
    expect(fixtures.drained.terminal).toBe(false);
    expect(fixtures.abandoned.terminal).toBe(false);
    expect(fixtures.disable.terminal).toBe(true);
    expect(fixtures.crash.terminal).toBe(false);
    expect(fixtures.crashTerminal.terminal).toBe(true);
    for (const crash of [fixtures.crash, fixtures.crashTerminal]) {
      expect(crash.drained).toBe(false);
      expect(crash.abandoned).toBe(0);
    }
  });

  test("are reachable from the TypeScript package", () => {
    // A path that silently resolves to nothing would make every assertion below
    // vacuous, so the suite proves it found the real directory before using it.
    const fixtures = readdirSync(GOLDEN_DIR).filter((name) =>
      name.endsWith(".json"),
    );
    expect(fixtures.length).toBeGreaterThan(0);
    expect(fixtures).toContain("error_body.json");
  });

  test("every fixture this suite relies on still exists under its own name", () => {
    // The list below is held HERE rather than derived from the directory, and
    // that is the whole point: a suite that asks the directory what it should
    // contain can never notice a fixture going missing. Deriving it would make
    // this test agree with any directory at all, including an empty one.
    //
    // Asserted in one direction only. A fixture disappearing or being renamed on
    // the Rust side breaks a vector this package reads, so it fails here. A
    // fixture being ADDED is not a failure -- the directory is observed, not
    // owned, and Rust pinning a new shape should not redden a client that does
    // not speak it. The unconsumed ones are reported below instead.
    const relied = [
      "error_body",
      "error_body_module_removed",
      "module_control_request_route_bind",
      "module_control_request_route_bind_without_consumer_capabilities",
      "module_control_request_health_check",
      "module_control_response_health_check",
      "module_control_response_route_bind_ack",
      "principal_direct",
      "principal_reserved",
      "principal_unverified",
      "route_target_internal_service",
      "route_target_management_surface",
      "route_target_tool_provider",
    ];
    const present = new Set(
      readdirSync(GOLDEN_DIR)
        .filter((name) => name.endsWith(".json"))
        .map((name) => name.slice(0, -".json".length)),
    );
    const absent = relied.filter((name) => !present.has(name));
    expect(absent).toEqual([]);

    // CONTROL: the comparison is only meaningful if `present` was actually
    // populated. Two empty sets agree, and that agreement would report a
    // missing directory as a clean pass.
    expect(present.size).toBeGreaterThanOrEqual(relied.length);
  });

  test("reports the fixtures no TypeScript test reads", () => {
    // Informational by design, and it does not assert a count. A fixture with no
    // consumer is a shape Rust pins and this package does not check -- worth
    // seeing when someone adds one, but not worth failing over, since the right
    // response is sometimes "the client does not speak that".
    const source = readFileSync(
      join(import.meta.dir, "golden-conformance.test.ts"),
      "utf8",
    );
    const unconsumed = readdirSync(GOLDEN_DIR)
      .filter((name) => name.endsWith(".json"))
      .map((name) => name.slice(0, -".json".length))
      .filter((name) => !source.includes(`"${name}"`));
    if (unconsumed.length > 0) {
      console.log(
        `golden fixtures with no TypeScript consumer: ${unconsumed.join(", ")}`,
      );
    }
    // The scan reads this file, so it must at minimum find the names above. A
    // read that returned nothing would report perfect coverage.
    expect(source.length).toBeGreaterThan(0);
  });

  test("error bodies carry the fields the client reads off a failed frame", () => {
    const body = loadGolden("error_body");
    const removed = loadGolden("error_body_module_removed");
    // errorFromFrame reads exactly these two, so a rename on the Rust side turns
    // every daemon error into the "subc error" fallback with its cause dropped:
    // a silent degradation rather than a failure, which is why it needs a test.
    expect(typeof body.code).toBe("string");
    expect(typeof body.message).toBe("string");
    expect(removed.code).toBe("module_removed");
    expect(typeof removed.message).toBe("string");
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

  test("principal variants keep the tag and payload the provider switches on", () => {
    // Principal is the DAEMON'S ATTESTATION of who the caller is -- the field a
    // provider reads to tell a spawn-attested module from anything else that
    // completed the handshake. provider.ts casts it (`request.principal as
    // Principal`) rather than parsing it, so a tag rename on the Rust side would
    // reach an `if (principal.kind === "reserved")` that silently never matches:
    // an authorization check that stops matching is a check that stops enforcing.
    const direct = loadGolden("principal_direct");
    const reserved = loadGolden("principal_reserved");
    const unverified = loadGolden("principal_unverified");

    expect(direct.kind).toBe("direct");
    expect(unverified.kind).toBe("unverified");
    expect(reserved.kind).toBe("reserved");
    // Only `reserved` carries a module id, and it is the whole content of the
    // attestation -- without it the variant names no one.
    expect(typeof reserved.module_id).toBe("string");
    expect(reserved.module_id).not.toBe("");
    // The other two must NOT carry one: a variant that grew a module id would be
    // a different claim wearing the same tag.
    expect(direct.module_id).toBeUndefined();
    expect(unverified.module_id).toBeUndefined();

    // Three DISTINCT tags. A fixture set that collapsed to one value would
    // satisfy every assertion above while proving nothing about discrimination.
    expect(new Set([direct.kind, reserved.kind, unverified.kind]).size).toBe(3);
  });

  test("a health report keeps the op and status the daemon dispatches on", () => {
    // provider.ts builds this response by hand -- it emits `op: "health.check"`
    // with `status`, and folds `detail`/`metrics` in only when defined. The
    // daemon reads `status` to decide whether a module is degraded and worth
    // escalating, so a rename on either side turns every reply into an
    // unrecognised shape while the module keeps answering promptly: the module
    // looks alive and its health stops being read.
    const request = loadGolden("module_control_request_health_check");
    const response = loadGolden("module_control_response_health_check");

    expect(request.op).toBe("health.check");
    expect(response.op).toBe("health.check");
    expect(typeof response.status).toBe("string");

    // `status` is a closed set on both sides -- the provider's HealthStatus union
    // and the daemon's escalation policy agree on these three, so a fourth value
    // arriving from Rust would be a policy change wearing a data change's shape.
    // `String(...)` narrows the `unknown` for toContain's string overload; the
    // line above already proved the runtime type.
    expect(["ok", "degraded", "failing"]).toContain(String(response.status));

    // The optional pair: the provider omits them when undefined rather than
    // sending nulls, and this fixture is the one that carries both, so it pins
    // the present form. The absent form is pinned by the request fixture above,
    // which carries neither -- two fixtures, opposite states, same field set.
    expect(typeof response.detail).toBe("string");
    expect(response.metrics).toBeDefined();
    expect(request.detail).toBeUndefined();
    expect(request.metrics).toBeUndefined();
  });

  test("a route bind ack is the bind op echoed back, not a new shape", () => {
    // The ACK is deliberately minimal: the daemon correlates it by frame corr and
    // reads only the op. That makes it the easiest shape to break silently --
    // there is no payload whose absence would be noticed, so a renamed op reaches
    // a daemon that treats the reply as unrecognised and the route as unbound,
    // with the module believing it accepted.
    const ack = loadGolden("module_control_response_route_bind_ack");
    const request = loadGolden("module_control_request_route_bind");

    expect(ack.op).toBe("route.bind");
    // The ack ECHOES the request's op rather than carrying one of its own, which
    // is the property that keeps them in step: comparing the literal to itself
    // would pass even if both drifted together.
    expect(ack.op).toBe(request.op);

    // CONTROL: the ack is the minimal form. If it grew fields, this pins that the
    // growth was deliberate rather than a fixture picking up the request's body.
    expect(Object.keys(ack)).toEqual(["op"]);
  });

  test("an absent consumer_capabilities stays absent rather than arriving empty", () => {
    // The provider treats an omitted field as "no reverse-request capability".
    // Omission and an empty array behave identically today but are different
    // bytes, and only the omitted form is pinned by a vector -- without one, a
    // field the daemon stops sending is indistinguishable from a field the
    // client stops reading. An optional field elsewhere in this protocol was
    // once misspelled on the wire and parsed cleanly as absent, silently
    // downgrading the caller's identity; that is the failure this pins against.
    const without = loadGolden(
      "module_control_request_route_bind_without_consumer_capabilities",
    );
    const with_ = loadGolden("module_control_request_route_bind");

    expect(without.op).toBe("route.bind");
    expect(without.consumer_capabilities).toBeUndefined();
    expect("consumer_capabilities" in without).toBe(false);

    // CONTROL: the sibling fixture proves the field is emitted when present, so
    // the absence above is a real absence and not a fixture that never carries it.
    expect(Array.isArray(with_.consumer_capabilities)).toBe(true);
    expect((with_.consumer_capabilities as string[]).length).toBeGreaterThan(0);

    // Both carry the fields the provider reads unconditionally, so a bind with no
    // declared capabilities is still a complete bind.
    for (const bind of [without, with_]) {
      expect(typeof bind.route_channel).toBe("number");
      expect(typeof bind.epoch).toBe("number");
      expect(typeof (bind.principal as Record<string, unknown>).kind).toBe(
        "string",
      );
    }
  });

  test("bind identity keeps the snake_case field the client sends", () => {
    // The client serializes this object by hand, so a casing change in Rust
    // would be accepted by TypeScript's types and rejected by the daemon.
    const identity = loadGolden("bind_identity");
    expect(typeof identity.project_root).toBe("string");
    expect(typeof identity.harness).toBe("string");
    expect(typeof identity.session).toBe("string");
  });

  test("catalog self-signal declarations decode from the Rust manifest vector", () => {
    const manifest = loadGolden("module_manifest_with_self_signals");
    const entry = JSON.parse(
      JSON.stringify({
        module_id: manifest.module_id,
        roles: [],
        control_ops: [],
        self_signals: manifest.self_signals,
      }),
    ) as CatalogEntry;

    expect(entry.self_signals).toHaveLength(2);
    expect(entry.self_signals?.[0]?.effect).toBe("observe");
    expect(entry.self_signals?.[1]?.effect).toBe("mutate");
    expect(entry.self_signals?.[1]?.anchored_to).toEqual({
      event: { event: "window_expiry" },
    });

    const legacyManifest = loadGolden("module_manifest_without_self_signals");
    const legacyEntry = JSON.parse(
      JSON.stringify({
        module_id: legacyManifest.module_id,
        roles: [],
        control_ops: [],
      }),
    ) as CatalogEntry;
    expect(legacyEntry.self_signals).toBeUndefined();
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
