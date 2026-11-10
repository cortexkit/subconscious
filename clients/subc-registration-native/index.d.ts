export interface NativeChannel {
  read(): Promise<Buffer>;
  write(bytes: Buffer): Promise<void>;
  close(): void;
}
export interface NativeHandoff {
  readonly environment: Record<string, string>;
  readonly childFd: number | null;
  complete(pid: number, claim: string): Promise<void>;
  abort(): void;
}
export function supervised(): boolean;
export function registrationRefusal(): string | null;
export function takeRegistrationChannel(): NativeChannel | null;
export function prepareHelperClaim(childSlot: number): NativeHandoff;
export function helperClaim(): Promise<string | null>;
