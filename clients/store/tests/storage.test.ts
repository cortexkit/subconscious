import { readFileSync } from "node:fs";

import { describe, expect, test } from "bun:test";

import {
  parseStorageDescriptor,
  postgresDatabaseName,
  sqliteStorePath,
  type StorageDescriptor,
} from "../src/index.js";

interface GoldenVector {
  module_id: string;
  postgres_database_name: string;
  sqlite_store_path: string;
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
