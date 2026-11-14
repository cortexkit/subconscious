import { createRequire } from "node:module";
import { ClaimError } from "./claim.js";
import type { Native } from "./registration-native.js";
export type { Native, NativeChannel } from "./registration-native.js";

let addon: Native | undefined;
export function native(): Native {
  if (addon) return addon;
  try {
    const provided = typeof require === "undefined" ? createRequire(import.meta.url) : require;
    return addon = provided("ck:registration") as Native;
  } catch (error) {
    throw new ClaimError("registration_helper_unavailable", undefined, error);
  }
}
