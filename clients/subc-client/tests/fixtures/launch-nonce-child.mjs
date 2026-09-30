// A stand-in for a module: reads its launch nonce through the SDK accessor and
// reports, as one JSON line on stdout, what it got and what the read did to
// descriptor 3 and to the environment. With "grandchild" as its argument it
// then spawns itself with "tool", as a module spawning a tool would, and
// includes the grandchild's report.
import { spawnSync } from "node:child_process";
import { closeSync, fstatSync, openSync, readFileSync } from "node:fs";

import { launchNonce } from "../../src/launch-nonce.ts";

const mode = process.argv[2] ?? "read";
const envBefore = JSON.stringify(process.env);

function attempt() {
  try {
    const nonce = launchNonce();
    return nonce === undefined ? { none: true } : { value: nonce.value, source: nonce.source };
  } catch (error) {
    return { error: error.kind, message: error.message };
  }
}

function fdState(fd) {
  try {
    const stat = fstatSync(fd, { bigint: true });
    return { open: true, fifo: stat.isFIFO(), inode: String(BigInt.asUintN(64, stat.ino)) };
  } catch (error) {
    return { open: false, code: error.code };
  }
}

const report = { mode };
report.fd3Before = fdState(3);
report.first = attempt();
report.fd3AfterFirst = fdState(3);

// Open an unrelated file. Once the pipe is closed, 3 is the lowest free
// number, so this file usually lands there: a second read that went back to
// the descriptor would find it instead of the pipe.
const other = openSync(process.argv[1], "r");
report.otherFd = other;
report.second = attempt();
report.sameAnswer = JSON.stringify(report.second) === JSON.stringify(report.first);
report.otherAfterSecond = fdState(other);
closeSync(other);

report.envUnchanged = JSON.stringify(process.env) === envBefore;

// After a refusal, whatever the pipe still holds shows that it was not consumed.
// Not in a grandchild: descriptor 3 there is one the runtime opened for itself
// (under both Bun and Node on macOS, something fstat reports as a FIFO with
// inode 0), and reading it fails or breaks the runtime.
if (mode !== "tool" && report.first.error !== undefined && report.fd3AfterFirst.open && report.fd3AfterFirst.fifo) {
  report.leftInPipe = readFileSync(3, "utf8");
}

if (mode === "grandchild") {
  const result = spawnSync(process.execPath, [...process.execArgv, process.argv[1], "tool"], {
    stdio: ["ignore", "pipe", "inherit"],
    encoding: "utf8",
  });
  report.grandchild = JSON.parse(result.stdout);
}

process.stdout.write(JSON.stringify(report));
