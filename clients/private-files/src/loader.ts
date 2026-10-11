import { createRequire } from 'node:module'
import type { PrivateKind } from './policy.js'

export interface PrivateFilesNative {
  create(path: string, kind: PrivateKind, bytes: Uint8Array): Promise<void>
  read(path: string): Promise<Buffer>
  verify(path: string, kind?: PrivateKind | 'auto'): Promise<void>
  secure(path: string, kind: PrivateKind): Promise<void>
}

export class PrivateFilesUnavailableError extends Error {
  readonly code = 'ERR_PRIVATE_FILES_UNAVAILABLE'
}

export function privateFilesBinary(arch: string, bunVersion?: string): string {
  // Select by Node/Bun process.arch, not the host CPU: emulated x64 Node needs the x64 binary.
  if (arch !== 'x64' && arch !== 'arm64') {
    throw new PrivateFilesUnavailableError(
      `Windows private files do not support ${arch}; use x64 or ARM64 Node 24. No mode-only fallback is safe.`,
    )
  }
  if (arch === 'arm64' && bunVersion && !supportsWindowsArmBun(bunVersion)) {
    throw new PrivateFilesUnavailableError(
      'Windows ARM64 requires Bun >= 1.3.10 with native ARM64 support; use ARM64 Node 24 or upgrade Bun. No mode-only fallback is safe.',
    )
  }
  return `private-files-win32-${arch}.node`
}

function supportsWindowsArmBun(version: string): boolean {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version)
  if (!match) return false
  const major = Number(match[1])
  const minor = Number(match[2])
  const patch = Number(match[3])
  return (
    major > 1 || (major === 1 && (minor > 3 || (minor === 3 && patch >= 10)))
  )
}

let backend: PrivateFilesNative | undefined
export function loadPrivateFilesNative(): PrivateFilesNative {
  if (backend) return backend
  const binary = privateFilesBinary(process.arch, process.versions.bun)
  try {
    backend = createRequire(import.meta.url)(`../prebuilds/${binary}`)
    if (
      !backend ||
      ['create', 'read', 'verify', 'secure'].some(
        (key) =>
          typeof backend?.[key as keyof PrivateFilesNative] !== 'function',
      )
    ) {
      throw new Error('Invalid private-files N-API interface')
    }
    return backend
  } catch (cause) {
    throw new PrivateFilesUnavailableError(
      `Cannot load ${binary}. Install @cortexkit/private-files with the matching Windows N-API v8 prebuild, or rebuild private-files/native with MSVC. Use local NTFS. No mode-only fallback is safe. Cause: ${String(cause)}`,
    )
  }
}
