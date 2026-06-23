import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const ROOT = join(import.meta.dir, "..", "..", "..");
export const DAEMON = join(ROOT, "target", "debug", "subc-core");
export const CONN_NAME = "subc-connection.json";

let buildChecked = false;

export interface LiveDaemon {
  connFile: string;
  runtimeDir: string;
  configDir: string;
  stderr: () => string;
  stop: () => void;
}

export function ensureSubcCoreBuilt(): void {
  if (buildChecked && existsSync(DAEMON)) return;
  if (!existsSync(DAEMON)) {
    const built = spawnSync("cargo", ["build", "-p", "subc-core", "--bins"], {
      cwd: ROOT,
      encoding: "utf8",
      stdio: "pipe",
    });
    if (built.status !== 0) {
      throw new Error(
        `cargo build -p subc-core --bins failed with status ${built.status ?? "unknown"}\n${built.stdout}${built.stderr}`,
      );
    }
  }
  if (!existsSync(DAEMON)) {
    throw new Error(`subc-core binary was not produced at ${DAEMON}`);
  }
  buildChecked = true;
}

export async function startLiveDaemon(prefix = "subc-live"): Promise<LiveDaemon> {
  ensureSubcCoreBuilt();

  const runtimeDir = mkdtempSync(join(tmpdir(), `${prefix}-rt-`));
  const configDir = mkdtempSync(join(tmpdir(), `${prefix}-cfg-`));
  const connFile = join(runtimeDir, CONN_NAME);
  let stderr = "";
  let exit: { code: number | null; signal: NodeJS.Signals | null } | null = null;

  const daemon: ChildProcess = spawn(DAEMON, [], {
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: runtimeDir,
      XDG_CONFIG_HOME: configDir,
      SUBC_PORT: "0",
    },
    stdio: ["ignore", "ignore", "pipe"],
  });
  daemon.stderr?.on("data", (chunk: Buffer) => {
    stderr += chunk.toString();
  });
  daemon.once("exit", (code, signal) => {
    exit = { code, signal };
  });

  try {
    await waitFor(() => existsSync(connFile) || exit !== null, 10_000, "daemon connection file");
    if (!existsSync(connFile)) {
      throw new Error(`subc-core exited before writing its connection file: ${JSON.stringify(exit)}\n${stderr}`);
    }
  } catch (err) {
    daemon.kill("SIGKILL");
    rmSync(runtimeDir, { recursive: true, force: true });
    rmSync(configDir, { recursive: true, force: true });
    throw err;
  }

  return {
    connFile,
    runtimeDir,
    configDir,
    stderr: () => stderr,
    stop: () => {
      daemon.kill("SIGKILL");
      rmSync(runtimeDir, { recursive: true, force: true });
      rmSync(configDir, { recursive: true, force: true });
    },
  };
}

export async function waitFor(predicate: () => boolean, timeoutMs: number, label: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${label}`);
}
