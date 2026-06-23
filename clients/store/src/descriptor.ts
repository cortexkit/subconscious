export interface StorageDescriptor {
  module_id: string;
  storage_namespace: string;
  isolation: Isolation;
  backend: StorageBackend;
}

export interface Isolation {
  kind: "module";
}

export type StorageBackend = SqliteStorageBackend | PostgresStorageBackend;

export interface SqliteStorageBackend {
  backend: "sqlite";
  path: string;
}

export interface PostgresStorageBackend {
  backend: "postgres";
  dsn: string;
  database: string;
}

type JsonRecord = Record<string, unknown>;

export function parseStorageDescriptor(value: unknown): StorageDescriptor {
  const descriptor = expectRecord(value, "storage descriptor");
  const moduleId = expectString(descriptor.module_id, "storage descriptor.module_id");
  const storageNamespace = expectString(
    descriptor.storage_namespace,
    "storage descriptor.storage_namespace",
  );
  const isolation = parseIsolation(descriptor.isolation);
  const backend = parseBackend(descriptor.backend);

  return {
    module_id: moduleId,
    storage_namespace: storageNamespace,
    isolation,
    backend,
  };
}

function parseIsolation(value: unknown): Isolation {
  const isolation = expectRecord(value, "storage descriptor.isolation");
  const kind = expectString(isolation.kind, "storage descriptor.isolation.kind");
  if (kind !== "module") {
    throw descriptorError('storage descriptor.isolation.kind must be "module"');
  }
  return { kind };
}

function parseBackend(value: unknown): StorageBackend {
  const backend = expectRecord(value, "storage descriptor.backend");
  const kind = expectString(backend.backend, "storage descriptor.backend.backend");

  if (kind === "sqlite") {
    return {
      backend: kind,
      path: expectString(backend.path, "storage descriptor.backend.path"),
    };
  }

  if (kind === "postgres") {
    return {
      backend: kind,
      dsn: expectString(backend.dsn, "storage descriptor.backend.dsn"),
      database: expectString(backend.database, "storage descriptor.backend.database"),
    };
  }

  throw descriptorError('storage descriptor.backend.backend must be "sqlite" or "postgres"');
}

function expectRecord(value: unknown, path: string): JsonRecord {
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    return value as JsonRecord;
  }
  throw descriptorError(`${path} must be an object`);
}

function expectString(value: unknown, path: string): string {
  if (typeof value === "string") {
    return value;
  }
  throw descriptorError(`${path} must be a string`);
}

function descriptorError(message: string): Error {
  return new Error(`Invalid StorageDescriptor: ${message}`);
}
