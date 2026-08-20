import { readFileSync } from "node:fs";

import { describe, expect, test } from "bun:test";

import {
  moduleStorePath,
  parseStorageDescriptor,
  postgresDatabaseName,
  sqliteStorePath,
  type StorageDescriptor,
} from "../src/index.js";

interface GoldenVector {
  module_id: string;
  postgres_database_name: string;
  sqlite_store_path: string;
  sqlite_descriptor: unknown;
}

interface GoldenFixture {
  data_home: string;
  vectors: GoldenVector[];
}

const fixture = JSON.parse(
  readFileSync(new URL("./golden/storage_vectors.json", import.meta.url), "utf8"),
) as GoldenFixture;

describe("storage derivation parity", () => {
  test("matches every vendored Rust golden vector", () => {
    for (const vector of fixture.vectors) {
      expect(postgresDatabaseName(vector.module_id)).toBe(vector.postgres_database_name);
      expect(sqliteStorePath(fixture.data_home, vector.module_id)).toBe(vector.sqlite_store_path);
      // The descriptor in the shared fixture must parse and round-trip identically
      // (the descriptor wire shape is part of the same cross-language contract).
      const descriptor = parseStorageDescriptor(vector.sqlite_descriptor);
      expect(JSON.parse(JSON.stringify(descriptor))).toEqual(vector.sqlite_descriptor);
    }
  });

  test("keeps slug-colliding module ids on distinct postgres databases", () => {
    const databaseNames = new Map(
      fixture.vectors.map((vector) => [vector.module_id, vector.postgres_database_name]),
    );

    expect(databaseNames.get("a-b")).toBeDefined();
    expect(databaseNames.get("a_b")).toBeDefined();
    expect(databaseNames.get("a-b")).not.toBe(databaseNames.get("a_b"));
  });
});

describe("parseStorageDescriptor", () => {
  test("round-trips sqlite and postgres descriptors through JSON", () => {
    const descriptors: StorageDescriptor[] = [
      {
        module_id: "alfonso-routing",
        storage_namespace: "default",
        isolation: { kind: "module" },
        backend: { backend: "sqlite", path: "/data/cortexkit/alfonso-routing/store.db" },
      },
      {
        module_id: "ai-provider-quota",
        storage_namespace: "default",
        isolation: { kind: "module" },
        backend: {
          backend: "postgres",
          dsn: "postgres://example.invalid/cortexkit",
          database: "cortexkit_ai_provider_quota_40f7c994078b5902",
        },
      },
    ];

    for (const descriptor of descriptors) {
      expect(parseStorageDescriptor(JSON.parse(JSON.stringify(descriptor)))).toEqual(descriptor);
    }
  });

  test("throws a clear error on a malformed descriptor", () => {
    expect(() =>
      parseStorageDescriptor({
        module_id: "alfonso-routing",
        storage_namespace: "default",
        isolation: { kind: "module" },
      }),
    ).toThrow(/storage descriptor\.backend must be an object/);
  });
});

describe("sqliteStorePath path-hazard refusal (issue #32)", () => {
  // The derivation REFUSES ids unusable as a path component instead of
  // sanitizing them: sanitizing would silently re-path deployed stores and
  // diverge from the daemon's Rust derivation. Each case asserts the REASON so
  // a broken predicate cannot pass by throwing the wrong refusal.
  test("refuses traversal, collision-class, and unprintable ids by name", () => {
    const cases: Array<[string, RegExp]> = [
      ["../escape", /path separator/],
      ["a/b", /path separator/],
      ["a\\b", /path separator/],
      ["..", /dot path component/],
      [".", /dot path component/],
      ["", /empty/],
      ["evil\u0000id", /control character/],
    ];
    for (const [moduleId, reason] of cases) {
      expect(() => sqliteStorePath("/data", moduleId)).toThrow(reason);
    }
  });

  test("every working fleet id shape passes byte-identically", () => {
    // The refusal must not move a single deployed path: hyphens, dots inside
    // names, and reserved-namespace colons all pass through verbatim.
    expect(sqliteStorePath("/data", "magic-context")).toBe(
      "/data/cortexkit/magic-context/store.db",
    );
    expect(sqliteStorePath("/data", "mcp:everything")).toBe(
      "/data/cortexkit/mcp:everything/store.db",
    );
    expect(sqliteStorePath("/data", "v1.2-module")).toBe("/data/cortexkit/v1.2-module/store.db");
  });
});

describe("data-home resolver (parity with cortexkit-store-types 0.2.0)", () => {
  test("moduleStorePath honours absolute XDG_DATA_HOME", () => {
    expect(moduleStorePath("astrocyte", { XDG_DATA_HOME: "/tmp/xdg-test" })).toBe(
      "/tmp/xdg-test/cortexkit/astrocyte/store.db",
    );
  });

  test("moduleStorePath defaults to HOME/.local/share", () => {
    expect(moduleStorePath("astrocyte", { HOME: "/tmp/home-test" })).toBe(
      "/tmp/home-test/.local/share/cortexkit/astrocyte/store.db",
    );
  });

  test("relative XDG_DATA_HOME is ignored per the basedir spec", () => {
    expect(moduleStorePath("m", { XDG_DATA_HOME: "relative/path", HOME: "/tmp/home-test" })).toBe(
      "/tmp/home-test/.local/share/cortexkit/m/store.db",
    );
  });

  test("feeding a module dir as a data home doubles the nesting (the astrocyte defect, pinned)", () => {
    // Negative example kept as a fence on the low-level contract: this is what
    // the two-argument form does when handed an already-qualified module
    // directory, and why moduleStorePath exists.
    expect(sqliteStorePath("/x/.local/share/cortexkit/astrocyte", "astrocyte")).toBe(
      "/x/.local/share/cortexkit/astrocyte/cortexkit/astrocyte/store.db",
    );
  });
});
