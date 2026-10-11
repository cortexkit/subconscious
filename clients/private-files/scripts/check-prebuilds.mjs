import { readFile } from 'node:fs/promises'
import { resolve, sep } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = process.argv[2]
  ? pathToFileURL(`${resolve(process.argv[2])}${sep}`)
  : new URL('../prebuilds/', import.meta.url)
for (const arch of ['x64', 'arm64']) {
  const bytes = await readFile(new URL(`private-files-win32-${arch}.node`, root))
  if (bytes[0] !== 0x4d || bytes[1] !== 0x5a) throw new Error(`Missing PE binary: ${arch}`)
  const pe = bytes.readUInt32LE(0x3c)
  const expected = arch === 'x64' ? 0x8664 : 0xaa64
  if (bytes.readUInt32LE(pe) !== 0x4550 || bytes.readUInt16LE(pe + 4) !== expected) {
    throw new Error(`Wrong PE machine architecture: ${arch}`)
  }
  const optional = pe + 24
  if (bytes.readUInt16LE(optional) !== 0x20b) throw new Error('Expected PE32+ prebuild')
  const sectionTable = optional + bytes.readUInt16LE(pe + 20)
  const sectionCount = bytes.readUInt16LE(pe + 6)
  function offset(rva) {
    for (let i = 0; i < sectionCount; i++) {
      const section = sectionTable + 40 * i
      const start = bytes.readUInt32LE(section + 12)
      const size = Math.max(bytes.readUInt32LE(section + 8), bytes.readUInt32LE(section + 16))
      if (rva >= start && rva < start + size) return bytes.readUInt32LE(section + 20) + rva - start
    }
    throw new Error(`Invalid PE RVA: ${rva}`)
  }
  function imports(index, stride, nameOffset) {
    const rva = bytes.readUInt32LE(optional + 112 + 8 * index)
    if (!rva) return []
    const names = []
    for (let at = offset(rva), i = 0; i < 256; at += stride, i++) {
      const name = bytes.readUInt32LE(at + nameOffset)
      if (!name) return names
      if (index === 13 && bytes.readUInt32LE(at) !== 1) throw new Error('Expected RVA delay imports')
      const start = offset(name)
      const end = bytes.indexOf(0, start)
      if (end < 0) throw new Error('Unterminated PE import name')
      names.push(bytes.toString('ascii', start, end).toLowerCase())
    }
    throw new Error('Unterminated PE import table')
  }
  if (imports(1, 20, 12).includes('node.exe') || !imports(13, 32, 4).includes('node.exe')) {
    throw new Error(`prebuild_requires_host_delay_import: win32-${arch} must delay-load node.exe for Bun/Node compatibility`)
  }
  console.log(`Verified packaged PE prebuild: win32-${arch}, host delay imports`)
}
