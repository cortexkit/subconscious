import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { performance } from 'node:perf_hooks'
import { after, before, test as nodeTest } from 'node:test'
import { fileURLToPath } from 'node:url'
import {
  createPrivateDirectory,
  createPrivateFile,
  readOwnerOnly,
  readPrivateFile,
  securePrivatePath,
  verifyOwnerOnly,
  verifyPrivatePath,
  verifySafeParent,
} from '../../dist/index.js'

assert.equal(
  process.platform,
  'win32',
  'This is a required real Windows test, not a portable mock',
)
const root = join(
  process.env.USERPROFILE,
  `private-files-test-${crypto.randomUUID()}`,
)
const reports = []
const latencies = []
const repairBeforeRemoval = []
// Allow 120 seconds per test in both Node and Bun for user-profile provisioning and credentialed probes.
const windowsTestOptions = { timeout: 120_000 }
const test = (name, body) => nodeTest(name, windowsTestOptions, body)
function nativeProof(name) {
  const binary = fileURLToPath(new URL('../../private-files-evidence/native-security.exe', import.meta.url))
  const result = spawnSync(binary, [name], { encoding: 'utf8', timeout: 60_000 })
  reports.push({ nativeProof: name, stdout: result.stdout, stderr: result.stderr, status: result.status })
  assert.ifError(result.error)
  assert.equal(result.status, 0, `${name}: ${result.stdout}\n${result.stderr}`)
  assert.match(result.stdout, new RegExp(`${name}: PASS \\(0 failures\\)`))
}

test('directory create keeps original identity and failure deletes only its handle', () => nativeProof('directory-identity'))
test('private create excludes concurrent readers and flushes before success', () => nativeProof('private-write'))
test('owner policy refuses nonprivileged default owners across elevation changes', () => nativeProof('owner-policy'))
test('N-API allocation queue and shutdown failures never use unset values', () => nativeProof('napi-failures'))
test('secret copies are zeroized on success and every failure path', () => nativeProof('zeroize'))
test('token sizing failures and allocation errors are checked', () => nativeProof('token-queries'))
test('reserved devices in every component refuse before opening', () => nativeProof('reserved-names'))
test('ACE and SID decoding stays within declared extents', () => nativeProof('bounded-aces'))
test('secure changes only its handle and leaves child ACLs unchanged', () => nativeProof('secure-handle'))
test('delay hook uses only the host executable without loader fallback', () => nativeProof('delay-hook'))
test('safe-parent unknown SID traverse-only and ACE semantics', () => nativeProof('safe-parent-aces'))

test('safe-parent accepts the real admin OpenCode config and inherit-only root grants', async () => {
  const config = join(process.env.USERPROFILE, '.config', 'opencode')
  await mkdir(config, { recursive: true })
  const profile = await inspect('inspect', { path: process.env.USERPROFILE })
  const appContainerTraverse = profile.aces.filter((ace) => ace.type === 0 && !(ace.flags & 8) && ace.sid.startsWith('S-1-15-3-') && ace.mask === 0x00100020)
  console.log(`admin profile AppContainer traverse entry present: ${appContainerTraverse.length > 0}`)
  reports.push({ profile: process.env.USERPROFILE, appContainerTraverse })
  assert.deepEqual(await verifySafeParent(config), { ok: true })
})

test('safe-parent refuses Users Modify and the runner temp tree', async () => {
  const parent = join(root, 'users-modify')
  await mkdir(parent)
  await inspect('fixture', { path: parent, kind: 'usersModify' })
  const result = await verifySafeParent(parent)
  assert.equal(result.ok, false)
  assert.equal(result.reason, 'writable-by-others')
  assert.equal(result.level.toLowerCase(), parent.toLowerCase())
  assert.equal(result.sid, 'S-1-5-32-545')
  const temp = await verifySafeParent(process.env.RUNNER_TEMP || 'D:\\a\\_temp')
  assert.equal(temp.ok, false)
  assert.equal(temp.reason, 'writable-by-others')
})

test('safe-parent refuses ancestor FILE_DELETE_CHILD', async () => {
  const ancestor = join(root, 'delete-child-ancestor')
  const parent = join(ancestor, 'config')
  await mkdir(parent, { recursive: true })
  await inspect('fixture', { path: ancestor, kind: 'usersDeleteChild' })
  const result = await verifySafeParent(parent)
  assert.equal(result.ok, false)
  assert.equal(result.reason, 'writable-by-others')
  assert.equal(result.level.toLowerCase(), ancestor.toLowerCase())
  assert.equal(result.mask, 0x40)
})

test('safe-parent refuses reparse points at parent and ancestor', async () => {
  const target = join(root, 'safe-parent-target')
  const link = join(root, 'safe-parent-junction')
  await mkdir(join(target, 'config'), { recursive: true })
  await inspect('fixture', { path: link, kind: 'junction', target })
  for (const path of [link, join(link, 'config')]) {
    const result = await verifySafeParent(path)
    assert.equal(result.ok, false)
    assert.equal(result.reason, 'reparse-point')
    assert.equal(result.level.toLowerCase(), link.toLowerCase())
  }
})

test('safe-parent refuses a parent owned by a non-trusted user', async () => {
  const parent = join(root, 'safe-parent-foreign-owner')
  await mkdir(parent)
  const owner = await inspect('fixture', { path: parent, kind: 'userOwner' })
  const result = await verifySafeParent(parent)
  assert.equal(result.ok, false)
  assert.equal(result.reason, 'wrong-owner')
  assert.equal(result.sid, owner.sid)
  assert.equal(result.level.toLowerCase(), parent.toLowerCase())
})
const script = fileURLToPath(new URL('./inspect.ps1', import.meta.url))
const powershell = join(
  process.env.SystemRoot,
  'System32',
  'WindowsPowerShell',
  'v1.0',
  'powershell.exe',
)

function inspect(mode, data) {
  return new Promise((resolve, reject) => {
    const started = performance.now()
    const child = spawn(
      powershell,
      [
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        script,
        '-Mode',
        mode,
      ],
      {
        windowsHide: true,
        stdio: ['pipe', 'pipe', 'pipe'],
        env: {
          ...process.env,
          PSModulePath: join(
            process.env.SystemRoot,
            'System32',
            'WindowsPowerShell',
            'v1.0',
            'Modules',
          ),
        },
      },
    )
    let stdout = '',
      stderr = ''
    const timeout = setTimeout(() => {
      child.kill()
      reject(new Error(`Inspector ${mode} timed out`))
    }, 60000)
    child.stdout.on('data', (bytes) => {
      stdout += bytes
      if (stdout.length > 1024 * 1024) child.kill()
    })
    child.stderr.on('data', (bytes) => {
      stderr += bytes
    })
    child.on('error', (error) => {
      clearTimeout(timeout)
      reject(error)
    })
    child.on('close', (code) => {
      clearTimeout(timeout)
      reports.push({
        inspector: mode,
        elapsedMs: performance.now() - started,
        exitCode: code,
      })
      if (code !== 0)
        reject(new Error(`Inspector ${mode} exited ${code}: ${stderr}`))
      else {
        try {
          resolve(JSON.parse(stdout.replace(/^\uFEFF/, '')))
        } catch (error) {
          reject(error)
        }
      }
    })
    child.stdin.end(JSON.stringify(data))
  })
}

async function canonical(path, directory, ownerSid) {
  const sd = await inspect('inspect', { path })
  reports.push({ name: path, descriptor: sd })
  assert.equal(sd.owner, ownerSid ?? sd.currentUser)
  assert.equal(sd.control & 0x1004, 0x1004, 'present protected DACL')
  assert.deepEqual(sd.aces, [
    { type: 0, flags: directory ? 3 : 0, mask: 0x1f01ff, sid: sd.currentUser },
  ])
}

async function refused(operation) {
  await assert.rejects(operation, (error) => {
    assert.equal(error.code, 'ERR_PRIVATE_FILES_SECURITY')
    assert.equal(typeof error.win32Code, 'number')
    reports.push({ refusal: error.message, win32Code: error.win32Code })
    return true
  })
}

before(async () => {
  reports.push({
    environment: await inspect('environment', {}),
    node: process.versions.node,
    bun: process.versions.bun ?? null,
    arch: process.arch,
  })
  const start = performance.now()
  await createPrivateDirectory(root)
  reports.push({ coldCreateMs: performance.now() - start })
})
after(async () => {
  const artifacts =
    process.env.PRIVATE_FILES_ARTIFACTS ?? 'private-files-evidence'
  await mkdir(artifacts, { recursive: true })
  latencies.sort((a, b) => a - b)
  const quantile = (p) =>
    latencies[Math.min(latencies.length - 1, Math.floor(latencies.length * p))]
  await writeFile(
    join(
      artifacts,
      `acl-${process.versions.bun ? `bun-${process.versions.bun}` : 'node'}-${process.arch}${process.env.PRIVATE_FILES_NEGATIVE_CONTROL ? '-negative' : ''}.json`,
    ),
    JSON.stringify(
      {
        reports,
        writes: latencies.length,
        timing: {
          p50: quantile(0.5),
          p95: quantile(0.95),
          p99: quantile(0.99),
        },
      },
      null,
      2,
    ),
  )
  for (const path of repairBeforeRemoval) await securePrivatePath(path, 'file')
  await rm(root, { recursive: true, force: true })
})

test('real ACL creation, independent descriptors, same-handle read and standard-user denial', async () => {
  const directory = join(root, 'private')
  const file = join(directory, 'token')
  await createPrivateDirectory(directory)
  await createPrivateFile(file, 'secret')
  await canonical(directory, true)
  await canonical(file, false)
  assert.equal((await readPrivateFile(file)).toString(), 'secret')
  const probe = await inspect('probe', {
    file,
    directory,
    runtime: process.execPath,
  })
  assert.equal(probe.standardUser, true)
  assert.equal(probe.controlRead, true)
  assert.equal(probe.fileDenied, true)
  assert.equal(probe.directoryDenied, true)
  assert.equal(probe.standardWriter, true)
  console.log(`standard-user profile AppContainer traverse entry present: ${probe.diagnostics.appContainerTraversePresent}`)
  reports.push({ probe })
})

test('foreign-grant fixture refuses before reading bytes', async () => {
  const file = join(root, 'foreign')
  await createPrivateFile(file, 'never consume')
  await inspect('fixture', { path: file, kind: 'foreign' })
  const sd = await inspect('inspect', { path: file })
  assert.ok(
    sd.aces.some((ace) => ace.sid === 'S-1-1-0'),
    'fixture must really grant Everyone',
  )
  await refused(() => readPrivateFile(file))
  await refused(() => verifyPrivatePath(file, 'file'))
  assert.deepEqual(await verifyOwnerOnly(file), {
    ok: false,
    refusal: { reason: 'extra-ace', sid: 'S-1-1-0', mask: 0x1f01ff },
  })
})

test('null-DACL, empty-DACL and foreign-owner fixtures refuse', async () => {
  for (const kind of ['null', 'empty', 'owner']) {
    const file = join(root, kind)
    await createPrivateFile(file, 'must not be read')
    if (kind === 'empty') repairBeforeRemoval.push(file)
    await inspect('fixture', { path: file, kind })
    const sd = await inspect('inspect', { path: file })
    reports.push({ fixture: kind, descriptor: sd })
    if (kind === 'null') assert.equal(sd.aces, null)
    if (kind === 'empty') assert.deepEqual(sd.aces, [])
    if (kind === 'owner') assert.equal(sd.owner, 'S-1-5-32-545')
    const result = await verifyOwnerOnly(file)
    assert.equal(result.ok, false)
    assert.equal(
      result.refusal.reason,
      kind === 'null'
        ? 'null-dacl'
        : kind === 'empty'
          ? 'empty-dacl'
          : 'wrong-owner',
    )
    if (kind === 'owner') assert.equal(result.refusal.ownerSid, sd.owner)
    const expectedReason =
      kind === 'null'
        ? 'null-dacl'
        : kind === 'empty'
          ? 'empty-dacl'
          : 'wrong-owner'
    await assert.rejects(
      () => readOwnerOnly(file),
      (error) =>
        error.code === 'ERR_PRIVATE_FILES_SECURITY' &&
        error.reason === expectedReason,
    )
    await refused(() => readPrivateFile(file))
    if (kind === 'owner') await refused(() => securePrivatePath(file, 'file'))
  }
  const administratorsFile = join(root, 'administrators-owner')
  await createPrivateFile(administratorsFile, 'approved owner bytes')
  await inspect('fixture', { path: administratorsFile, kind: 'administratorsOwner' })
  const sd = await inspect('inspect', { path: administratorsFile })
  assert.equal(sd.owner, 'S-1-5-32-544')
  assert.equal(sd.control & 0x1004, 0x1004, 'present protected DACL')
  assert.deepEqual(sd.aces, [
    { type: 0, flags: 0, mask: 0x1f01ff, sid: sd.currentUser },
  ])
  assert.deepEqual(await verifyOwnerOnly(administratorsFile), { ok: true })
  assert.equal((await readOwnerOnly(administratorsFile)).toString(), 'approved owner bytes')
})

test('secure repairs Administrators-owned inherited file and directory without taking ownership', async () => {
  const parent = join(root, 'administrators-repair')
  await createPrivateDirectory(parent)
  // Administrators may control ancestors, but their inherited ACE is not a private-file DACL.
  await inspect('fixture', { path: parent, kind: 'inheritableAdministrators' })
  for (const kind of ['file', 'directory']) {
    const path = join(parent, kind)
    if (kind === 'file') await writeFile(path, 'legacy bytes')
    else await mkdir(path)
    await inspect('fixture', { path, kind: 'administratorsOwner' })
    const before = await inspect('inspect', { path })
    assert.equal(before.owner, 'S-1-5-32-544')
    assert.equal(before.control & 0x1000, 0, 'fixture must have an unprotected DACL')
    assert.equal(before.aces.length, 2)
    assert.ok(before.aces.every((ace) => ace.flags & 16), 'fixture ACEs must be inherited')
    assert.equal((await verifyOwnerOnly(path)).refusal.reason, 'extra-ace')
    const child = kind === 'directory' ? join(path, 'legacy') : null
    let childBefore
    if (child) {
      await writeFile(child, 'legacy bytes')
      childBefore = await inspect('inspect', { path: child })
    }
    await securePrivatePath(path, kind)
    await canonical(path, kind === 'directory', before.owner)
    assert.deepEqual(await verifyOwnerOnly(path), { ok: true })
    if (child) {
      assert.deepEqual(await inspect('inspect', { path: child }), childBefore)
      await securePrivatePath(child, 'file')
      assert.deepEqual(await verifyOwnerOnly(child), { ok: true })
    }
    assert.equal((await readOwnerOnly(child ?? path)).toString(), 'legacy bytes')
  }
})

test('secure refuses a non-trusted user owner with its SID and leaves the DACL unchanged', async () => {
  const file = join(root, 'non-trusted-owner-repair')
  await createPrivateFile(file, 'unchanged bytes')
  const owner = await inspect('fixture', { path: file, kind: 'userOwner' })
  const before = await inspect('inspect', { path: file })
  assert.equal(before.owner, owner.sid)
  assert.notEqual(before.owner, before.currentUser)
  await assert.rejects(() => securePrivatePath(file, 'file'), (error) => {
    assert.equal(error.code, 'ERR_PRIVATE_FILES_SECURITY')
    assert.equal(error.reason, 'wrong-owner')
    assert.equal(error.ownerSid, owner.sid)
    assert.equal(error.win32Code, 5)
    return true
  })
  assert.deepEqual(await inspect('inspect', { path: file }), before)
  assert.deepEqual(await verifyOwnerOnly(file), {
    ok: false,
    refusal: { reason: 'wrong-owner', ownerSid: owner.sid },
  })
  assert.equal((await readFile(file)).toString(), 'unchanged bytes')
})

test('junction leaf and junction ancestor refuse', async () => {
  const target = join(root, 'target')
  await createPrivateDirectory(target)
  await createPrivateFile(join(target, 'token'), 'secret')
  const junction = join(root, 'junction')
  await inspect('fixture', { path: junction, kind: 'junction', target })
  const result = await verifyOwnerOnly(junction)
  assert.equal(result.ok, false)
  assert.equal(result.refusal.reason, 'reparse-point')
  await refused(() => verifyPrivatePath(junction, 'directory'))
  await refused(() => readPrivateFile(join(junction, 'token')))
  await refused(() =>
    createPrivateFile(join(junction, 'new'), 'must not write'),
  )
})

test('typed not-protected directory and unsupported-filesystem refusals', async () => {
  const directory = join(root, 'unprotected-dir')
  await mkdir(directory)
  const sd = await inspect('inspect', { path: directory })
  reports.push({ unprotectedDirectory: sd })
  assert.equal(sd.control & 0x1000, 0)
  const unprotected = await verifyOwnerOnly(directory)
  assert.equal(unprotected.ok, false)
  assert.equal(unprotected.refusal.reason, 'not-protected')
  const unsupported = await verifyOwnerOnly('\\\\localhost\\C$\\never-opened')
  assert.equal(unsupported.ok, false)
  assert.equal(unsupported.refusal.reason, 'unsupported-filesystem')
})

test('unsafe writable ancestor refuses private children and new creation', async () => {
  const directory = join(root, 'unsafe')
  await createPrivateDirectory(directory)
  const file = join(directory, 'token')
  await createPrivateFile(file, 'secret')
  await inspect('fixture', { path: directory, kind: 'foreign' })
  await refused(() => readPrivateFile(file))
  await refused(() =>
    createPrivateFile(join(directory, 'new'), 'must not write'),
  )
})

test('explicit current-owner migration and same-directory rename retain owner-only ACL and denial', async () => {
  const directory = join(root, 'migration')
  await createPrivateDirectory(directory)
  const file = join(directory, 'legacy')
  await createPrivateFile(file, 'legacy bytes')
  await inspect('fixture', { path: file, kind: 'foreign' })
  const before = await inspect('inspect', { path: file })
  assert.equal(before.owner, before.currentUser)
  await securePrivatePath(file, 'file')
  await canonical(file, false)
  assert.deepEqual(await verifyOwnerOnly(file), { ok: true })
  assert.equal((await readPrivateFile(file)).toString(), 'legacy bytes')
  const staging = join(directory, 'staging')
  await createPrivateFile(staging, 'replacement')
  await rename(staging, file)
  await verifyPrivatePath(file, 'file')
  await canonical(file, false)
  assert.equal((await readPrivateFile(file)).toString(), 'replacement')
  reports.push({
    afterMigrationAndRename: await inspect('probe', {
      file,
      directory,
      runtime: process.execPath,
    }),
  })
})

test('canonical inherited file and token default owner are accepted only in a private parent', async () => {
  const file = join(root, 'compatible-inherited')
  await createPrivateFile(file, 'compatible bytes')
  await inspect('fixture', { path: file, kind: 'inherited' })
  await inspect('fixture', { path: file, kind: 'defaultOwner' })
  const sd = await inspect('inspect', { path: file })
  assert.equal(sd.owner, sd.defaultOwner)
  assert.equal(sd.aces[0].flags, 16)
  assert.equal(sd.control & 0x1000, 0)
  reports.push({ compatibleInherited: sd })
  assert.deepEqual(await verifyOwnerOnly(file), { ok: true })
  assert.equal((await readOwnerOnly(file)).toString(), 'compatible bytes')
})

test('inherited foreign grant refuses', async () => {
  const directory = join(root, 'inherited-foreign-dir')
  await createPrivateDirectory(directory)
  const file = join(directory, 'token')
  await createPrivateFile(file, 'secret')
  await inspect('fixture', { path: directory, kind: 'inheritableForeign' })
  await inspect('fixture', { path: file, kind: 'inherited' })
  const sd = await inspect('inspect', { path: file })
  assert.ok(sd.aces.some((ace) => ace.sid === 'S-1-1-0' && ace.flags & 16))
  await refused(() => readOwnerOnly(file))
})

test('real Rust writers interoperate with typed verifier and same-handle reader', async () => {
  const executable = fileURLToPath(
    new URL(
      '../fixtures/rust/target/debug/private-files-rust-fixture.exe',
      import.meta.url,
    ),
  )
  await new Promise((resolve, reject) => {
    const child = spawn(executable, [root], { stdio: 'inherit' })
    child.on('error', reject)
    child.on('exit', (code) =>
      code === 0 ? resolve() : reject(new Error(`Rust fixture exited ${code}`)),
    )
  })
  const fixtures = [
    ['rust-subc-file', 'rust-subc'],
    ['rust-lease-dir/protected-file', 'rust-lease'],
    ['rust-lease-dir/inherited-file', 'rust-inherited'],
  ]
  const directory = join(root, 'rust-lease-dir')
  reports.push({
    rustDirectory: await inspect('inspect', { path: directory }),
  })
  assert.deepEqual(await verifyOwnerOnly(directory), { ok: true })
  for (const [name, expected] of fixtures) {
    const file = join(root, name)
    const sd = await inspect('inspect', { path: file })
    reports.push({ rustFixture: name, descriptor: sd })
    assert.deepEqual(
      await verifyOwnerOnly(file),
      { ok: true },
      `Rust ACL refused: ${JSON.stringify(sd)}`,
    )
    assert.equal(
      (await readOwnerOnly(file)).toString(),
      expected,
      `Rust ACL: ${JSON.stringify(sd)}`,
    )
  }
})

test('warm secure writes yield to the event loop and record latency', async () => {
  const intervalMs = 10
  const minimumWorkMs = 1000
  const maximumDelayMs = 250
  let ticks = 0,
    interleavedTicks = 0,
    inFlight = 0,
    maxDelayMs = 0
  let lastTick = performance.now()
  const timer = setInterval(() => {
    const now = performance.now()
    maxDelayMs = Math.max(maxDelayMs, now - lastTick - intervalMs)
    lastTick = now
    ticks++
    // Credit this interval callback as interleaved only while createPrivateFile promises are pending.
    if (inFlight > 0) interleavedTicks++
  }, intervalMs)
  let rounds = 0
  const payload = Buffer.alloc(1024 * 1024, 42)
  try {
    await new Promise((resolve) => setTimeout(resolve, 2 * intervalMs))
    const start = performance.now()
    do {
      const files = Array.from({ length: 32 }, (_, index) =>
        join(root, `timing-${index}`),
      )
      await Promise.all(
        files.map(async (file) => {
          const began = performance.now()
          inFlight++
          try {
            await createPrivateFile(file, payload)
          } finally {
            inFlight--
          }
          latencies.push(performance.now() - began)
        }),
      )
      await Promise.all(
        files.map(async (file) =>
          assert.equal((await readFile(file)).length, payload.length),
        ),
      )
      await Promise.all(files.map((file) => rm(file)))
      rounds++
    } while (performance.now() - start < minimumWorkMs)
    await new Promise((resolve) => setTimeout(resolve, 2 * intervalMs))
  } finally {
    clearInterval(timer)
  }
  const measurement = {
    intervalMs,
    minimumWorkMs,
    maximumDelayMs,
    maxDelayMs,
    ticks,
    interleavedTicks,
    rounds,
  }
  reports.push({ eventLoop: measurement })
  assert.ok(
    interleavedTicks > 0,
    `No timer interleaved with pending native writes: ${JSON.stringify(measurement)}`,
  )
  assert.ok(
    maxDelayMs < maximumDelayMs,
    `Event-loop delay exceeded budget: ${JSON.stringify(measurement)}`,
  )
})
