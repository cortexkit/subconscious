import { constants } from 'node:fs'
import { chmod, lstat, mkdir, open } from 'node:fs/promises'
import {
  loadPrivateFilesNative,
  PrivateFilesUnavailableError,
} from './loader.js'
import type { PrivateKind } from './policy.js'
import {
  checkPosixOwnerOnlyPath,
  checkPosixOwnerOnlyPathAsync,
  type PosixOwnerOnlyPathOptions,
  type PosixOwnerOnlyPathAsyncOptions,
  type PosixOwnerOnlyPathResult,
} from './posix-path.js'

export { PrivateFilesUnavailableError } from './loader.js'
export {
  isPrivateDescriptor,
  type PrivateDescriptor,
  type PrivateKind,
} from './policy.js'
export * from './refusals.js'
export * from './posix-path.js'

export type SafeParentResult =
  | { ok: true }
  | { ok: false; reason: string; path: string; level?: string; sid?: string; mask?: number }

function posixParentResult(path: string, result: PosixOwnerOnlyPathResult): SafeParentResult {
  if (result.ok) return result
  const writable = /writable|sticky|user-private group/.test(result.reason)
  return {
    ok: false,
    reason: writable ? 'writable-by-others' : result.reason === 'foreign owner'
      ? 'wrong-owner' : result.reason === 'ancestor is not a directory' ? 'wrong-kind' : 'io-error',
    path,
    level: result.path,
  }
}

/** POSIX-only synchronous form; Windows requires handle-based asynchronous verification. */
export function verifySafeParentSync(path: string, options: PosixOwnerOnlyPathOptions = {}): SafeParentResult {
  if (process.platform === 'win32') return { ok: false, reason: 'unsupported-filesystem', path }
  return posixParentResult(path, checkPosixOwnerOnlyPath(path, { ...options, directoryOnly: true }))
}

/** Check whether a directory and its ancestors protect entries from untrusted writers. */
export async function verifySafeParent(path: string, options: PosixOwnerOnlyPathAsyncOptions = {}): Promise<SafeParentResult> {
  if (process.platform !== 'win32') {
    return posixParentResult(path, await checkPosixOwnerOnlyPathAsync(path, { ...options, directoryOnly: true }))
  }
  try {
    await loadPrivateFilesNative().verifySafeParent(path)
    return { ok: true }
  } catch (error) {
    if (error instanceof PrivateFilesUnavailableError) return { ok: false, reason: 'unsupported-filesystem', path }
    const native = error as Error & { code?: string; reason?: string; level?: string; sid?: string; ownerSid?: string; mask?: number }
    if (native.code !== 'ERR_PRIVATE_FILES_SECURITY') throw error
    return {
      ok: false, reason: native.reason ?? 'io-error', path,
      ...(native.level ? { level: native.level } : {}),
      ...(native.sid || native.ownerSid ? { sid: native.sid ?? native.ownerSid } : {}),
      ...(native.mask !== undefined ? { mask: native.mask } : {}),
    }
  }
}

/** Exclusive creation; Windows checks owner/DACL/reparse status before writing through the creation handle. */
export async function createPrivateFile(
  path: string,
  bytes: Uint8Array | string = '',
): Promise<void> {
  if (process.platform === 'win32') {
    return loadPrivateFilesNative().create(
      path,
      'file',
      typeof bytes === 'string' ? Buffer.from(bytes) : bytes,
    )
  }
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(bytes)
  } finally {
    await handle.close()
  }
}

/** Creates one directory, without silently tightening an existing shared parent. */
export async function createPrivateDirectory(path: string): Promise<void> {
  if (process.platform === 'win32') {
    return loadPrivateFilesNative().create(path, 'directory', Buffer.alloc(0))
  }
  await mkdir(path, { mode: 0o700 })
}

export async function verifyPrivatePath(
  path: string,
  kind: PrivateKind,
): Promise<void> {
  if (process.platform === 'win32') {
    return loadPrivateFilesNative().verify(path, kind)
  }
  const metadata = await lstat(path)
  if (
    (kind === 'file' ? !metadata.isFile() : !metadata.isDirectory()) ||
    (metadata.mode & 0o777) !== (kind === 'file' ? 0o600 : 0o700) ||
    metadata.uid !== process.getuid?.()
  ) {
    throw new Error(
      `Private ${kind} requires current-user ownership and ${kind === 'file' ? '0600' : '0700'}: ${path}`,
    )
  }
}

/** Windows verifies and reads the same pinned handle, never a pathname reopen. */
export async function readPrivateFile(path: string): Promise<Buffer> {
  if (process.platform === 'win32') return loadPrivateFilesNative().read(path)
  // Nonblocking also keeps special files from hanging before the kind check.
  const handle = await open(
    path,
    constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
  )
  try {
    const metadata = await handle.stat()
    if (
      !metadata.isFile() ||
      (metadata.mode & 0o777) !== 0o600 ||
      metadata.uid !== process.getuid?.()
    ) {
      throw new Error(`Private file requires current-user ownership and 0600: ${path}`)
    }
    return await handle.readFile()
  } finally {
    await handle.close()
  }
}

/** Replaces permissions on an existing object; Windows only modifies objects owned by the process user. */
export async function securePrivatePath(
  path: string,
  kind: PrivateKind,
): Promise<void> {
  if (process.platform === 'win32')
    return loadPrivateFilesNative().secure(path, kind)
  await chmod(path, kind === 'file' ? 0o600 : 0o700)
}

export type OwnerOnlyRefusal =
  | { reason: 'wrong-owner'; ownerSid: string }
  | { reason: 'extra-ace'; sid: string; mask: number }
  | {
      reason:
        | 'not-protected'
        | 'null-dacl'
        | 'empty-dacl'
        | 'reparse-point'
        | 'unsupported-filesystem'
        | 'unsafe-ancestor'
        | 'wrong-kind'
        | 'io-error'
      message: string
    }
  | { reason: 'mode'; actual: number; expected: number }

export type OwnerOnlyResult =
  | { ok: true }
  | { ok: false; refusal: OwnerOnlyRefusal }

export async function createOwnerOnly(
  path: string,
  options: { kind: 'file' | 'dir' },
): Promise<void> {
  if (options.kind === 'dir') return createPrivateDirectory(path)
  return createPrivateFile(path)
}

/** Windows inspects a pinned handle; refusal details never include file contents. */
export async function verifyOwnerOnly(path: string): Promise<OwnerOnlyResult> {
  if (process.platform !== 'win32') {
    const metadata = await lstat(path)
    const expected = metadata.isDirectory() ? 0o700 : 0o600
    if (!metadata.isFile() && !metadata.isDirectory()) {
      return {
        ok: false,
        refusal: {
          reason: 'wrong-kind',
          message: 'Expected a regular file or directory',
        },
      }
    }
    if (metadata.uid !== process.getuid?.()) {
      return {
        ok: false,
        refusal: { reason: 'wrong-owner', ownerSid: `uid:${metadata.uid}` },
      }
    }
    const actual = metadata.mode & 0o777
    return actual === expected
      ? { ok: true }
      : { ok: false, refusal: { reason: 'mode', actual, expected } }
  }
  try {
    await loadPrivateFilesNative().verify(path, 'auto')
    return { ok: true }
  } catch (error) {
    if (error instanceof PrivateFilesUnavailableError) {
      return {
        ok: false,
        refusal: { reason: 'unsupported-filesystem', message: error.message },
      }
    }
    const native = error as Error & {
      code?: string
      reason?: OwnerOnlyRefusal['reason']
      ownerSid?: string
      sid?: string
      mask?: number
    }
    if (native.code !== 'ERR_PRIVATE_FILES_SECURITY') throw error
    if (native.reason === 'wrong-owner')
      return {
        ok: false,
        refusal: {
          reason: 'wrong-owner',
          ownerSid: native.ownerSid ?? 'unknown',
        },
      }
    if (native.reason === 'extra-ace')
      return {
        ok: false,
        refusal: {
          reason: 'extra-ace',
          sid: native.sid ?? 'unknown',
          mask: native.mask ?? 0,
        },
      }
    return {
      ok: false,
      refusal: {
        reason:
          native.reason && native.reason !== 'mode'
            ? native.reason
            : 'io-error',
        message: native.message,
      },
    }
  }
}

export const readOwnerOnly = readPrivateFile
