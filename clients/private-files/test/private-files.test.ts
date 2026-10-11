import { afterEach, describe, expect, spyOn, test } from 'bun:test'
import { chmod, chown, mkdtemp, rm, stat, symlink } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  createOwnerOnly,
  createPrivateDirectory,
  createPrivateFile,
  readOwnerOnly,
  readPrivateFile,
  securePrivatePath,
  verifyOwnerOnly,
  verifyPrivatePath,
} from '../src/index.js'
import { loadPrivateFilesNative, privateFilesBinary } from '../src/loader.js'
import { isPrivateDescriptor, type PrivateDescriptor } from '../src/policy.js'

const descriptor: PrivateDescriptor = {
  owner: 'S-1-user',
  currentUser: 'S-1-user',
  defaultOwner: 'S-1-default',
  kind: 'file',
  reparse: false,
  daclPresent: true,
  daclProtected: true,
  aces: [{ type: 0, flags: 0, mask: 0x1f01ff, sid: 'S-1-user' }],
}

describe('private descriptor contract (logic only, not Windows certification)', () => {
  test('accepts exactly canonical file and directory templates', () => {
    expect(isPrivateDescriptor(descriptor, 'file')).toBe(true)
    expect(
      isPrivateDescriptor(
        {
          ...descriptor,
          kind: 'directory',
          aces: [{ ...descriptor.aces![0]!, flags: 3 }],
        },
        'directory',
      ),
    ).toBe(true)
  })
  test('accepts approved owners independently of token default owner and bounds inherited files to a private directory', () => {
    expect(
      isPrivateDescriptor({ ...descriptor, owner: 'S-1-default' }, 'file'),
    ).toBe(false)
    for (const owner of ['S-1-5-32-544', 'S-1-5-18']) {
      expect(isPrivateDescriptor({ ...descriptor, owner }, 'file')).toBe(true)
    }
    const parent: PrivateDescriptor = {
      ...descriptor,
      kind: 'directory',
      aces: [{ ...descriptor.aces![0]!, flags: 3 }],
    }
    const inherited: PrivateDescriptor = {
      ...descriptor,
      daclProtected: false,
      aces: [{ ...descriptor.aces![0]!, flags: 16 }],
      parent,
    }
    expect(isPrivateDescriptor(inherited, 'file')).toBe(true)
    expect(
      isPrivateDescriptor(
        {
          ...inherited,
          parent: {
            ...parent,
            currentUser: 'S-1-other',
            owner: 'S-1-other',
            aces: [{ type: 0, flags: 3, mask: 0x1f01ff, sid: 'S-1-other' }],
          },
        },
        'file',
      ),
    ).toBe(false)
    expect(
      isPrivateDescriptor(
        {
          ...inherited,
          parent: { ...parent, defaultOwner: 'S-1-other-default' },
        },
        'file',
      ),
    ).toBe(true)
    expect(
      isPrivateDescriptor({ ...inherited, parent: undefined }, 'file'),
    ).toBe(false)
    expect(
      isPrivateDescriptor(
        { ...inherited, parent: { ...parent, daclProtected: false } },
        'file',
      ),
    ).toBe(false)
  })
  test('refuses noncanonical descriptors', () => {
    for (const change of [
      { owner: 'foreign' },
      { kind: 'directory' as const },
      { reparse: true },
      { daclPresent: false },
      { daclProtected: false },
      { aces: null },
      { aces: [] },
      { aces: [descriptor.aces![0]!, descriptor.aces![0]!] },
      ...[
        { sid: 'foreign' },
        { flags: 16 },
        { flags: 3 },
        { type: 1 },
        { type: 9 },
        { mask: 1 },
      ].map((ace) => ({ aces: [{ ...descriptor.aces![0]!, ...ace }] })),
    ])
      expect(isPrivateDescriptor({ ...descriptor, ...change }, 'file')).toBe(
        false,
      )
  })
})

test('loader selects runtime arch including x64 emulation on ARM64 Windows', () => {
  expect(privateFilesBinary('x64')).toBe('private-files-win32-x64.node')
  expect(privateFilesBinary('arm64')).toBe('private-files-win32-arm64.node')
  expect(privateFilesBinary('x64', '1.3.14')).toBe(
    'private-files-win32-x64.node',
  )
  expect(privateFilesBinary('arm64', '1.3.14')).toBe(
    'private-files-win32-arm64.node',
  )
  expect(privateFilesBinary('arm64', '1.4.2')).toBe(
    'private-files-win32-arm64.node',
  )
})
test('loader refuses unsupported architectures and unqualified ARM64 Bun', () => {
  expect(() => privateFilesBinary('ia32')).toThrow('No mode-only fallback')
  expect(() => privateFilesBinary('arm64', '1.3.9')).toThrow('Node 24')
  expect(() => privateFilesBinary('arm64', 'unknown')).toThrow(
    'No mode-only fallback',
  )
})

const roots: string[] = []
afterEach(async () => {
  await Promise.all(
    roots.splice(0).map((root) => rm(root, { recursive: true, force: true })),
  )
})

describe.skipIf(process.platform === 'win32')(
  'private files POSIX compatibility',
  () => {
    test('loader refuses a missing or incompatible addon instead of a mode fallback', () => {
      expect(() => loadPrivateFilesNative()).toThrow('No mode-only fallback')
    })
    test('exclusive 0600 file and 0700 directory, same-handle read and explicit chmod', async () => {
      const root = await mkdtemp(join(tmpdir(), 'private-files-'))
      roots.push(root)
      const dir = join(root, 'private')
      const file = join(dir, 'token')
      await createPrivateDirectory(dir)
      await createPrivateFile(file, 'private bytes')
      expect((await stat(dir)).mode & 0o777).toBe(0o700)
      expect(await verifyOwnerOnly(dir)).toEqual({ ok: true })
      expect(await verifyOwnerOnly(file)).toEqual({ ok: true })
      const empty = join(dir, 'empty')
      await createOwnerOnly(empty, { kind: 'file' })
      expect(await readOwnerOnly(empty)).toEqual(Buffer.alloc(0))
      expect((await stat(file)).mode & 0o777).toBe(0o600)
      expect((await readPrivateFile(file)).toString()).toBe('private bytes')
      await verifyPrivatePath(dir, 'directory')
      await verifyPrivatePath(file, 'file')
      await expect(createPrivateFile(file, 'overwrite')).rejects.toThrow()
      await chmod(file, 0o644)
      expect(await verifyOwnerOnly(file)).toEqual({
        ok: false,
        refusal: { reason: 'mode', actual: 0o644, expected: 0o600 },
      })
      await expect(readPrivateFile(file)).rejects.toThrow('0600')
      await expect(verifyPrivatePath(file, 'file')).rejects.toThrow('0600')
      await expect(verifyPrivatePath(dir, 'file')).rejects.toThrow('0600')
      await securePrivatePath(file, 'file')
      expect((await readPrivateFile(file)).toString()).toBe('private bytes')
    })
    test('POSIX refuses symlink reads and verification even for a private target', async () => {
      const root = await mkdtemp(join(tmpdir(), 'private-files-'))
      roots.push(root)
      const file = join(root, 'token')
      const dir = join(root, 'private')
      await createPrivateFile(file, 'secret')
      await createPrivateDirectory(dir)
      for (const [target, kind] of [[file, 'file'], [dir, 'directory']] as const) {
        const link = `${target}-link`
        await symlink(target, link)
        await expect(verifyPrivatePath(link, kind)).rejects.toThrow()
        expect((await verifyOwnerOnly(link)).ok).toBe(false)
        await expect(readPrivateFile(link)).rejects.toThrow()
      }
    })
    test.skipIf(process.getuid?.() !== 0)('POSIX refuses foreign-owned 0600 files and 0700 directories', async () => {
      const root = await mkdtemp(join(tmpdir(), 'private-files-'))
      roots.push(root)
      const file = join(root, 'token')
      const dir = join(root, 'private')
      await createPrivateFile(file, 'foreign secret')
      await createPrivateDirectory(dir)
      for (const [path, kind] of [[file, 'file'], [dir, 'directory']] as const) {
        await chown(path, 65534, 65534)
        await expect(verifyPrivatePath(path, kind)).rejects.toThrow('ownership')
        expect(await verifyOwnerOnly(path)).toEqual({ ok: false, refusal: { reason: 'wrong-owner', ownerSid: 'uid:65534' } })
      }
      await expect(readPrivateFile(file)).rejects.toThrow('ownership')
    })
    test('POSIX compares file ownership to the calling process uid', async () => {
      const root = await mkdtemp(join(tmpdir(), 'private-files-'))
      roots.push(root)
      const file = join(root, 'token')
      await createPrivateFile(file, 'secret')
      const uid = process.getuid!()
      const caller = spyOn(process, 'getuid').mockReturnValue(uid + 1)
      try {
        await expect(readPrivateFile(file)).rejects.toThrow('ownership')
        await expect(verifyPrivatePath(file, 'file')).rejects.toThrow('ownership')
        expect(await verifyOwnerOnly(file)).toEqual({ ok: false, refusal: { reason: 'wrong-owner', ownerSid: `uid:${uid}` } })
      } finally {
        caller.mockRestore()
      }
    })
  },
)
