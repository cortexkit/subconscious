import { chmodSync, existsSync, mkdtempSync, mkdirSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { describe, expect, test } from "bun:test";

import {
  checkOwnerOnlyPath,
  checkOwnerOnlyPathAsync,
  type OwnerOnlyPathFileSystem,
  type OwnerOnlyPathMetadata,
  type OwnerOnlyPathOptions,
  type OwnerOnlyPathResult,
} from "../src/index.js";

function directory(mode = 0o700, uid = 10, gid = 20): OwnerOnlyPathMetadata {
  return { mode, uid, gid, isDirectory: () => true };
}

function ioError(code: string): Error & { code: string } {
  return Object.assign(new Error(code), { code });
}

const passwd = "# comment\n\nalice:x:10:20::/:/bin/sh\n";
const group = "# comment\n\nalice:x:20:\n";

function fileSystem() {
  const paths = posix;
  const key = (path: string) => paths.normalize(path);
  const nodes = new Map<string, { path: string; value: OwnerOnlyPathMetadata }>();
  const links = new Map<string, string>();
  const calls: string[] = [];
  const files = new Map<string, Uint8Array>([
    ["/etc/passwd", Buffer.from(passwd)], ["/etc/group", Buffer.from(group)],
  ]);
  function add(path: string, value = directory()) {
    nodes.set(key(path), { path, value });
  }
  function canonical(path: string): string {
    for (const [link, destination] of links) {
      if (key(path) === link || key(path).startsWith(`${link}${paths.sep}`)) {
        return canonical(destination + paths.normalize(path).slice(link.length));
      }
    }
    const node = nodes.get(key(path));
    if (!node) throw ioError("ENOENT");
    return node.path;
  }
  const fs: OwnerOnlyPathFileSystem = {
    realpathSync(path) { calls.push(`realpath:${path}`); return canonical(path); },
    lstatSync(path) {
      calls.push(`lstat:${path}`);
      const value = nodes.get(key(path))?.value;
      if (!value) throw ioError("ENOENT");
      return value;
    },
    statSync(path) { calls.push(`stat:${path}`); return nodes.get(key(canonical(path)))!.value; },
    readFileSync(path) {
      calls.push(`readFile:${path}`);
      const value = files.get(path);
      if (!value) throw ioError("ENOENT");
      return value;
    },
  };
  return { fs, add, files, calls, link(path: string, target: string) { add(path); links.set(key(path), target); } };
}

async function checkBoth(path: string, options: OwnerOnlyPathOptions): Promise<OwnerOnlyPathResult> {
  const sync = checkOwnerOnlyPath(path, options);
  const fs = options.fs!;
  const async = await checkOwnerOnlyPathAsync(path, {
    platform: options.platform, env: options.env, euid: options.euid, egid: options.egid,
    fs: {
      realpath: async (path) => fs.realpathSync!(path),
      lstat: async (path) => fs.lstatSync!(path),
      stat: async (path) => fs.statSync!(path),
      readFile: async (path) => fs.readFileSync!(path),
    },
  });
  expect(async).toEqual(sync);
  return sync;
}

function unixTree() {
  const fixture = fileSystem();
  fixture.add("/", directory(0o755, 0, 0));
  fixture.add("/home", directory(0o755, 0, 0));
  fixture.add("/home/alice");
  fixture.add("/home/alice/private");
  return { ...fixture, options: { platform: "linux", euid: 10, egid: 20, fs: fixture.fs } };
}


function expectRefusal(result: OwnerOnlyPathResult, reason: string, path?: string) {
  expect(result.ok).toBe(false);
  if (result.ok) throw new Error("expected a permission refusal");
  expect(result.reason).toContain(reason);
  if (path) expect(result.path).toBe(path);
}

describe("Unix owner-only paths", () => {
  test("Unix accepts an owner-only tree", async () => {
    const tree = unixTree();
    expect(await checkBoth("/home/alice/private/token.json", tree.options)).toEqual({ ok: true });
    const calls = tree.calls.slice(0, tree.calls.length / 2);
    for (const path of ["/home/alice/private", "/home/alice", "/home", "/"]) {
      expect(calls).toContain(`lstat:${path}`);
      expect(calls.indexOf(`stat:${path}`, calls.indexOf(`lstat:${path}`))).toBeGreaterThan(calls.indexOf(`lstat:${path}`));
    }
    expect(calls.some((call) => call.startsWith("readFile:"))).toBe(false);
  });

  test("Unix refuses a group-writable ancestor", async () => {
    const tree = unixTree();
    tree.add("/home", directory(0o775, 0, 0));
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "group-writable ancestor", "/home");
  });

  test("Unix refuses a world-writable ancestor", async () => {
    const tree = unixTree();
    tree.add("/home", directory(0o777, 0, 0));
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "world-writable ancestor", "/home");
  });

  test("Unix refuses a foreign owner even without write bits", async () => {
    const tree = unixTree();
    tree.add("/home/alice", directory(0o700, 11));
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "foreign owner", "/home/alice");
  });

  test("Unix accepts sticky tmp only with an owner-only immediate child", async () => {
    const tree = unixTree();
    tree.add("/tmp", directory(0o1777, 0, 0));
    tree.add("/tmp/alice");
    expect(await checkBoth("/tmp/alice/missing/nested/token.json", tree.options)).toEqual({ ok: true });
  });

  test("Unix refuses sticky tmp with a public immediate child", async () => {
    const tree = unixTree();
    tree.add("/tmp", directory(0o1777, 0, 0));
    tree.add("/tmp/alice", directory(0o775));
    tree.add("/tmp/alice/private");
    expectRefusal(await checkBoth("/tmp/alice/private/token.json", tree.options), "sticky ancestor without an owner-only child", "/tmp");
  });

  test("Unix refuses non-root sticky public directories", async () => {
    const tree = unixTree();
    tree.add("/home/alice", directory(0o1777));
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "world-writable ancestor");
  });

  test("Unix refuses a symlink into a public directory", async () => {
    const tree = unixTree();
    tree.add("/public", directory(0o777, 0, 0));
    tree.link("/home/alice/link", "/public");
    expectRefusal(await checkBoth("/home/alice/link/token.json", tree.options), "world-writable ancestor", "/public");
  });

  test("Unix checks an existing directory itself and an existing file's parent", async () => {
    const tree = unixTree();
    tree.add("/home/alice/private/token.json", { ...directory(0o666), isDirectory: () => false });
    expect(await checkBoth("/home/alice/private/token.json", tree.options)).toEqual({ ok: true });
    tree.add("/home/alice/private", directory(0o777));
    expectRefusal(await checkBoth("/home/alice/private", tree.options), "world-writable ancestor");
  });

  test("Unix refuses an unavailable effective identity", async () => {
    const tree = unixTree();
    expectRefusal(await checkBoth("/home/alice/private/token.json", { ...tree.options, euid: null }), "could not determine effective uid");
  });

  test("Unix refuses failed inspection of a resolved ancestor", async () => {
    const tree = unixTree();
    const lstat = tree.fs.lstatSync;
    tree.fs.lstatSync = (path) => { if (path === "/home") throw ioError("EACCES"); return lstat(path); };
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "could not inspect ancestor", "/home");
  });
});

interface GroupOverride {
  passwd?: string;
  group?: string;
  missing?: "passwd" | "group";
  unreadable?: "passwd" | "group";
  mode?: number;
  uid?: number;
  gid?: number;
  egid?: number | null;
}

const groupCases: Array<[string, string | undefined, GroupOverride]> = [
  ["private group", undefined, {}],
  ["self member", undefined, { group: "alice:x:20:alice\n" }],
  ["other member", "other group members", { group: "alice:x:20:bob\n" }],
  ["other primary", "another user's primary group", { passwd: "alice:x:10:20::/:/bin/sh\nbob:x:11:20::/:/bin/sh\n" }],
  ["absent uid", "absent or ambiguous", { passwd: "bob:x:11:21::/:/bin/sh\n" }],
  ["absent gid", "absent or ambiguous", { group: "bob:x:21:\n" }],
  ["missing passwd", "could not read /etc/passwd", { missing: "passwd" }],
  ["missing group", "could not read /etc/group", { missing: "group" }],
  ["unreadable passwd", "could not read /etc/passwd", { unreadable: "passwd" }],
  ["unreadable group", "could not read /etc/group", { unreadable: "group" }],
  ["world writable", "world-writable ancestor", { mode: 0o777 }],
  ["other owner", "foreign owner", { uid: 11 }],
  ["other gid", "group-writable ancestor", { gid: 21 }],
  ["unavailable egid", "group-writable ancestor", { egid: null }],
  ["malformed passwd", "could not parse /etc/passwd", { passwd: "broken\n" }],
  ["malformed group", "could not parse /etc/group", { group: "broken\n" }],
  ["duplicate uid", "absent or ambiguous", { passwd: "alice:x:10:20::/:/bin/sh\nbob:x:10:21::/:/bin/sh\n" }],
  ["duplicate gid", "absent or ambiguous", { group: "alice:x:20:\nbob:x:20:\n" }],
  ["invalid primary gid", "could not parse /etc/passwd", { passwd: "alice:x:10:NaN::/:/bin/sh\n" }],
  ["invalid group member", "could not parse /etc/group", { group: "alice:x:20:alice,\n" }],
];

describe("Unix user-private groups", () => {
  for (const [name, reason, override] of groupCases) {
    test(`user-private group ${reason ? "refuses" : "accepts"} ${name}`, async () => {
      const tree = unixTree();
      tree.add("/home/alice/private", directory(override.mode ?? 0o775, override.uid ?? 10, override.gid ?? 20));
      tree.files.set("/etc/passwd", Buffer.from(override.passwd ?? passwd));
      tree.files.set("/etc/group", Buffer.from(override.group ?? group));
      if (override.missing) tree.files.delete(`/etc/${override.missing}`);
      if (override.unreadable) {
        const read = tree.fs.readFileSync;
        tree.fs.readFileSync = (path) => {
          if (path === `/etc/${override.unreadable}`) throw ioError("EISDIR");
          return read(path);
        };
      }
      const result = await checkBoth("/home/alice/private/token.json", {
        ...tree.options, egid: override.egid === undefined ? 20 : override.egid,
      });
      if (reason) expectRefusal(result, reason, "/home/alice/private");
      else expect(result).toEqual({ ok: true });
    });
  }

  test("user-private group refuses invalid UTF-8", async () => {
    const tree = unixTree();
    tree.add("/home/alice/private", directory(0o775));
    tree.files.set("/etc/passwd", Uint8Array.from([0xff]));
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "could not parse /etc/passwd");
  });
});


describe("resolution errors", () => {
  test("refuses a dangling symlink rather than treating it as missing", async () => {
    const tree = unixTree();
    tree.link("/home/alice/private/token.json", "/missing");
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "could not resolve");
  });

  test("refuses non-ENOENT realpath failures", async () => {
    const tree = unixTree();
    tree.fs.realpathSync = () => { throw ioError("EACCES"); };
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "could not resolve");
  });

  test("refuses a non-directory nearest existing ancestor", async () => {
    const tree = unixTree();
    tree.add("/home/alice/private", { ...directory(), isDirectory: () => false });
    expectRefusal(await checkBoth("/home/alice/private/token.json", tree.options), "could not resolve");
  });

  test("refuses empty and NUL-containing paths", async () => {
    const tree = unixTree();
    for (const path of ["", "/home/alice/\0token"]) {
      expectRefusal(await checkBoth(path, tree.options), "could not resolve");
    }
  });
});

test.skipIf(process.platform === "win32")("real Unix filesystem checks do not create missing paths and resolve symlinks", async () => {
  const base = mkdtempSync(join(tmpdir(), "owner-only-path-"));
  try {
    chmodSync(base, 0o700);
    const missing = join(base, "missing", "token.json");
    expect(checkOwnerOnlyPath(missing)).toEqual({ ok: true });
    expect(await checkOwnerOnlyPathAsync(missing)).toEqual({ ok: true });
    expect(existsSync(join(base, "missing"))).toBe(false);
    const publicDir = join(base, "public");
    mkdirSync(publicDir);
    chmodSync(publicDir, 0o777);
    symlinkSync(publicDir, join(base, "link"));
    expectRefusal(checkOwnerOnlyPath(join(base, "link", "token.json")), "world-writable ancestor");
    expectRefusal(await checkOwnerOnlyPathAsync(join(base, "link", "token.json")), "world-writable ancestor");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
