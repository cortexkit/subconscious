import type { PosixOwnerOnlyPathFileSystem, PosixOwnerOnlyPathMetadata } from '../../src/posix-path.js'

/** Model user-owned 0775 config directories created with umask 0002. Inject
 * account files so private-group membership does not depend on the test host. */
export function ubuntuPrivateGroup(secondMember = false) {
  const directory = (mode: number, uid = 1000, gid = 1000): PosixOwnerOnlyPathMetadata =>
    ({ mode, uid, gid, isDirectory: () => true })
  const nodes = new Map([
    ['/', directory(0o755, 0, 0)],
    ['/home', directory(0o755, 0, 0)],
    ['/home/alice', directory(0o755)],
    ['/home/alice/.config', directory(0o775)],
    ['/home/alice/.config/opencode', directory(0o775)],
  ])
  const missing = () => { throw Object.assign(new Error('missing'), { code: 'ENOENT' }) }
  const metadata = (path: string) => nodes.get(path) ?? missing()
  const fs: PosixOwnerOnlyPathFileSystem = {
    realpathSync: (path) => { metadata(path); return path },
    lstatSync: metadata,
    statSync: metadata,
    readFileSync: (path) => path === '/etc/passwd'
      ? Buffer.from('root:x:0:0::/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\nbob:x:1001:1001::/home/bob:/bin/sh\n')
      : path === '/etc/group' ? Buffer.from(`root:x:0:\nalice:x:1000:${secondMember ? 'alice,bob' : ''}\nbob:x:1001:\n`) : missing(),
  }
  return {
    path: '/home/alice/.config', nodes,
    sync: { euid: 1000, egid: 1000, fs },
    async: { euid: 1000, egid: 1000, fs: {
      realpath: async (path: string) => fs.realpathSync(path),
      lstat: async (path: string) => fs.lstatSync(path),
      stat: async (path: string) => fs.statSync(path),
      readFile: async (path: string) => fs.readFileSync(path),
    } },
  }
}
