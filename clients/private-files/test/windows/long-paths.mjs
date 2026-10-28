import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { access, rename, rm } from 'node:fs/promises'
import { join, relative, toNamespacedPath } from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import {
  createOwnerOnly,
  createPrivateFile,
  readPrivateFile,
  securePrivatePath,
  verifyOwnerOnly,
  verifySafeParent,
} from '../../dist/index.js'

assert.equal(process.platform, 'win32', 'Long-path qualification requires real Windows')

test('Windows path conversion preserves namespaces and maps UNC without a share', async () => {
  const binary = fileURLToPath(new URL('../../private-files-evidence/native-security.exe', import.meta.url))
  const result = spawnSync(binary, ['path-conversion'], { encoding: 'utf8', timeout: 60_000 })
  assert.ifError(result.error)
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`)
  assert.match(result.stdout, /path-conversion: PASS \(0 failures\)/)
  for (const path of ['\\\\.\\C:\\private-files\\token', '\\\\server\\share\\token', '\\\\?\\UNC\\server\\share\\token']) {
    await assert.rejects(readPrivateFile(path), { reason: 'unsupported-filesystem', path })
  }
})

test('Windows long paths over 300 characters cover private file lifecycle', { timeout: 120_000 }, async () => {
  const root = join(process.env.USERPROFILE, `private-files-long-${crypto.randomUUID()}`)
  try {
    await createOwnerOnly(root, { kind: 'dir' })
    let parent = root
    while (parent.length <= 310) {
      parent = join(parent, 'private-directory-'.padEnd(60, 'd'))
      await createOwnerOnly(parent, { kind: 'dir' })
    }
    // The final directory creation and subsequent leaf opens both walk a long ancestor.
    parent = join(parent, 'containing-directory')
    await createOwnerOnly(parent, { kind: 'dir' })
    assert.ok(parent.length > 300)
    assert.deepEqual(await verifyOwnerOnly(parent), { ok: true })
    assert.deepEqual(await verifySafeParent(parent), { ok: true })

    const file = join(parent, 'owner.json')
    await createPrivateFile(file, 'first private value')
    assert.deepEqual(await verifyOwnerOnly(file), { ok: true })
    assert.equal((await readPrivateFile(file)).toString(), 'first private value')
    await securePrivatePath(file, 'file')
    await securePrivatePath(parent, 'directory')

    const extended = toNamespacedPath(file)
    assert.ok(extended.startsWith('\\\\?\\'))
    assert.deepEqual(await verifyOwnerOnly(extended), { ok: true })
    assert.deepEqual(await verifySafeParent(toNamespacedPath(parent)), { ok: true })
    assert.equal((await readPrivateFile(extended)).toString(), 'first private value')

    const renamed = join(parent, 'renamed.json')
    await rename(file, renamed)
    assert.equal((await readPrivateFile(renamed)).toString(), 'first private value')
    await createPrivateFile(file, 'replacement private value')
    await rename(file, renamed)
    assert.equal((await readPrivateFile(renamed)).toString(), 'replacement private value')
    await rm(renamed)
    await assert.rejects(access(renamed), { code: 'ENOENT' })

    const normalized = join(parent, 'normalized.json')
    const callerPath = `${relative(process.cwd(), parent).replaceAll('\\', '/')}/./unused/../normalized.json`
    await createOwnerOnly(callerPath, { kind: 'file' })
    assert.deepEqual(await verifyOwnerOnly(normalized), { ok: true })
    await rm(normalized)
    await assert.rejects(readPrivateFile(callerPath), (error) => {
      assert.equal(error.code, 'ERR_PRIVATE_FILES_SECURITY')
      assert.equal(error.path, callerPath)
      return true
    })
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test('Windows private file creation at exactly 260 characters', { timeout: 120_000 }, async () => {
  const root = join(process.env.USERPROFILE, `private-files-boundary-${crypto.randomUUID()}`)
  try {
    await createOwnerOnly(root, { kind: 'dir' })
    const parent = join(root, 'p'.repeat(249 - root.length - 1))
    assert.equal(parent.length, 249)
    await createOwnerOnly(parent, { kind: 'dir' })
    const file = join(parent, 'owner.json')
    assert.equal(file.length, 260)
    await createPrivateFile(file, 'boundary value')
    assert.deepEqual(await verifyOwnerOnly(file), { ok: true })
    assert.equal((await readPrivateFile(file)).toString(), 'boundary value')
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})
