import { createRequire } from "node:module";

export interface NativeChannel { read(): Promise<Buffer>; write(bytes: Buffer): Promise<void>; close(): void }
interface NativeHandoff {
  environment: Record<string, string>; childFd: number | null;
  complete(pid: number, claim: string): Promise<void>; abort(): void;
}
export interface Native {
  supervised(): boolean; registrationRefusal(): string | null;
  takeRegistrationChannel(): NativeChannel | null;
  prepareHelperClaim(slot: number): NativeHandoff;
  helperClaim(): Promise<string | null>;
}
let addon: Native | undefined;
export function native(): Native {
  if (addon) return addon;
  // The runner provides its embedded helper; ordinary programs use local addons.
  try {
    const provided = typeof require === "undefined" ? createRequire(import.meta.url) : require;
    return addon = provided("ck:registration") as Native;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "MODULE_NOT_FOUND") throw error;
  }
  if (!["darwin", "linux", "win32"].includes(process.platform) || !["arm64", "x64"].includes(process.arch)) {
    throw new Error(`registration native platform unsupported: ${process.platform}-${process.arch}`);
  }
  // Node ESM has no require; Bun's own require must stay visible to its bundler.
  if (typeof require === "undefined") {
    return addon = createRequire(import.meta.url)(`../native/subc-registration-${process.platform}-${process.arch}.node`) as Native;
  }
  // Literal paths let Bun embed the addon in a standalone compiled program.
  // An unresolved require in a try block is optional, so host-only builds work.
  try {
    switch (`${process.platform}-${process.arch}`) {
      case "darwin-arm64": return addon = require("../native/subc-registration-darwin-arm64.node") as Native;
      case "darwin-x64": return addon = require("../native/subc-registration-darwin-x64.node") as Native;
      case "linux-arm64": return addon = require("../native/subc-registration-linux-arm64.node") as Native;
      case "linux-x64": return addon = require("../native/subc-registration-linux-x64.node") as Native;
      case "win32-arm64": return addon = require("../native/subc-registration-win32-arm64.node") as Native;
      case "win32-x64": return addon = require("../native/subc-registration-win32-x64.node") as Native;
      default: throw new Error(`registration native platform unsupported: ${process.platform}-${process.arch}`);
    }
  } catch (error) { throw error; }
}
