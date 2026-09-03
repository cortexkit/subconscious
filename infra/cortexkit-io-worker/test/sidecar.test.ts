import { describe, expect, it } from "vitest";
import { parseSidecar, parseZipName } from "../src/assets";

const ABC_SHA256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

describe("parseSidecar", () => {
  it("reads the hash token from a shasum line", () => {
    expect(parseSidecar(`${ABC_SHA256}  ck-subc-darwin-arm64.zip\n`)).toBe(ABC_SHA256);
  });

  it("reads a bare hex digest", () => {
    expect(parseSidecar(`${ABC_SHA256.toUpperCase()}\n`)).toBe(ABC_SHA256);
  });

  it("returns null when no 64-hex token is present", () => {
    expect(parseSidecar("")).toBeNull();
    expect(parseSidecar("not-a-hash")).toBeNull();
    expect(parseSidecar("deadbeef")).toBeNull();
  });
});

describe("parseZipName", () => {
  it("parses hyphenated binary names from the trailing os-arch suffix", () => {
    expect(parseZipName("ck-subc-mcp-darwin-arm64.zip")).toEqual({
      binary: "ck-subc-mcp",
      os: "darwin",
      arch: "arm64",
    });
  });

  it("ignores sidecars and other filenames", () => {
    expect(parseZipName("ck-subc-darwin-arm64.zip.sha256")).toBeNull();
    expect(parseZipName("release-manifest.json")).toBeNull();
    expect(parseZipName("ck-subc-darwin-ppc.zip")).toBeNull();
  });
});
