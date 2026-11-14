import { createRequire } from "node:module";
import { Duplex } from "node:stream";
import { ClaimBroker, ClaimError, readClaimRefusal, readClaimPresentReply } from "./claim.js";
import { SubcSocket } from "./socket.js";
import { FrameType, Priority, buildFlags, buildFrame, encodeFrame } from "./envelope.js";

/** SDK-local registration failures, not daemon-authored claim refusal codes. */
export type RegistrationErrorCode = "registration_helper_unavailable" | "registration_adoption_failed"
  | "registration_already_taken" | "registration_owner_conflict" | "registration_not_ready" | "registration_closed";

interface NativeChannel { read(): Promise<Buffer>; write(bytes: Buffer): Promise<void>; close(): void }
interface NativeHandoff {
  environment: Record<string, string>; childFd: number | null;
  complete(pid: number, claim: string): Promise<void>; abort(): void;
}
interface Native {
  supervised(): boolean; registrationRefusal(): string | null;
  takeRegistrationChannel(): NativeChannel | null;
  prepareHelperClaim(slot: number): NativeHandoff;
  helperClaim(): Promise<string | null>;
}
let addon: Native | undefined;
function native(): Native {
  if (addon) return addon;
  if (!["darwin", "linux", "win32"].includes(process.platform) || !["arm64", "x64"].includes(process.arch)) {
    throw new Error(`registration native platform unsupported: ${process.platform}-${process.arch}`);
  }
  // Node ESM has no require; Bun's own require must stay visible to its bundler.
  if (typeof require === "undefined") {
    return addon = createRequire(import.meta.url)(`../native/subc-registration-${process.platform}-${process.arch}.node`) as Native;
  }
  // Literal paths let Bun embed the addon when compiling a standalone module.
  // Bun's bundler treats an unresolvable require inside a try block as optional
  // instead of failing the build, so a development checkout that built only its
  // own platform's binary still compiles. The catch only rethrows.
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

/** Native reads and writes remain on the N-API runtime, including
 * Windows duplex pipes using asynchronous I/O. Node never opens a second handle under the pipe name. */
class RegistrationDuplex extends Duplex {
  constructor(private readonly channel: NativeChannel) { super(); }
  override _read(): void {
    void this.channel.read().then(bytes => {
      if (bytes.length === 0) this.push(null); else this.push(bytes);
    }, () => this.destroy(new Error("registration channel read failed")));
  }
  override _write(bytes: Buffer, _encoding: BufferEncoding, callback: (error?: Error | null) => void): void {
    void this.channel.write(bytes).then(() => callback(), () => callback(new Error("registration channel write failed")));
  }
  override _destroy(error: Error | null, callback: (error: Error | null) => void): void {
    this.channel.close(); callback(error);
  }
}

export function registrationNamed(): boolean {
  return process.platform === "win32" ? process.env.SUBC_REGISTRATION_PIPE !== undefined : process.env.SUBC_REGISTRATION_FD !== undefined;
}

/** There is one owner even when two SDK module copies are imported. The native
 * decision is process-cached too, and refuses a second transfer of the handle. */
class RegistrationOwner {
  supervised = false;
  private decided = false;
  private adoptionError?: ClaimError;
  private socket?: SubcSocket;
  private taken = false;
  helperTaken = false;
  broker?: ClaimBroker;
  adopt(): void {
    if (this.adoptionError) throw this.adoptionError;
    if (this.decided) return;
    this.decided = true;
    if (!registrationNamed()) return;
    let helper: Native;
    try { helper = native(); }
    catch (error) {
      this.adoptionError = new ClaimError("registration_helper_unavailable", undefined, error);
      throw this.adoptionError;
    }
    this.supervised = helper.supervised();
    if (this.supervised) {
      const channel = helper.takeRegistrationChannel();
      if (!channel) throw new ClaimError("registration_adoption_failed");
      const stream = new RegistrationDuplex(channel);
      this.socket = SubcSocket.adopt(stream);
    }
  }
  take(): SubcSocket | undefined {
    if (!this.supervised) return undefined;
    if (this.taken) throw new ClaimError("registration_already_taken");
    this.taken = true;
    return this.socket!;
  }
  bind(request: ConstructorParameters<typeof ClaimBroker>[0]): void {
    if (this.broker || !this.supervised) throw new ClaimError("registration_owner_conflict");
    this.broker = new ClaimBroker(request);
  }
}
const key = Symbol.for("@cortexkit/subc-client.registration-owner/v1");
const globals = globalThis as typeof globalThis & { [key]?: RegistrationOwner };
export function registrationOwner(): RegistrationOwner { return globals[key] ??= new RegistrationOwner(); }

export async function bindConsumer(sock: SubcSocket, deadline: number): Promise<void> {
  const owner = registrationOwner();
  let claim: string | undefined;
  if (owner.supervised) {
    if (!owner.broker) throw new ClaimError("registration_not_ready");
    claim = await owner.broker.mint({purpose: "self"});
  } else if (process.env.SUBC_HELPER_CLAIM_FD !== undefined || process.env.SUBC_HELPER_CLAIM_PIPE !== undefined) {
    if (owner.helperTaken) throw new ClaimError("helper_claim_already_taken");
    owner.helperTaken = true;
    claim = await native().helperClaim() ?? undefined;
    if (!claim) throw new ClaimError("helper_claim_unavailable");
  }
  if (!claim) return;
  try {
    const frame = buildFrame(FrameType.Request, buildFlags(false, Priority.Passive, false), 0, 0, 1n,
      new TextEncoder().encode(JSON.stringify({op: "claim.present", claim})));
    await sock.write(encodeFrame(frame), deadline);
    const response = await sock.readFrame(deadline, deadline);
    const body = JSON.parse(new TextDecoder().decode(response.body)) as { op?: string; code?: string; detail?: unknown };
    if (response.header.corr !== 1n || response.header.channel !== 0) throw new ClaimError("invalid_claim_reply");
    if (response.header.ty === FrameType.Error) throw readClaimRefusal(body);
    if (response.header.ty !== FrameType.Response) throw new ClaimError("invalid_claim_reply");
    readClaimPresentReply(body);
  } catch (error) { sock.close(); throw error; }
}

export function boundIdentityOverride(value: unknown): void {
  if (hasClaimBinding() && value !== undefined) throw new ClaimError("claim_identity_conflict");
}
export function hasClaimBinding(): boolean {
  const owner = registrationOwner();
  return owner.supervised || owner.helperTaken;
}

export interface HelperClaimHandoff {
  readonly environment: Readonly<Record<string, string>>;
  readonly childFd: number | null;
  /** Complete only after spawn emits its spawn event. Retain the ChildProcess
   * until completion, and call abort on its error event. No claim enters env. */
  complete(childPid: number): Promise<void>;
  abort(): void;
}

/** Reserve before child_process.spawn. Unix: put childFd at stdio[childSlot]
 * and merge environment; Windows: merge environment, leaving stdio unchanged. */
export function prepareHelperClaim(childSlot = 3): HelperClaimHandoff {
  const owner = registrationOwner();
  if (!owner.supervised || !owner.broker) throw new ClaimError("registration_not_ready");
  const prepared = native().prepareHelperClaim(childSlot);
  let active = true;
  return {
    environment: Object.freeze(prepared.environment), childFd: prepared.childFd,
    async complete(pid) {
      if (!active || !Number.isSafeInteger(pid) || pid < 1) throw new ClaimError("invalid_helper_handoff");
      active = false;
      try { await prepared.complete(pid, await owner.broker!.mint({purpose: "helper"})); }
      catch (error) { prepared.abort(); throw error; }
    },
    abort() { active = false; prepared.abort(); },
  };
}
