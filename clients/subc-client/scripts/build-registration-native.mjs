import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';

const root = fileURLToPath(new URL('../../../', import.meta.url));
const triple = process.platform === 'win32' ? `${process.arch === 'x64' ? 'x86_64' : 'aarch64'}-pc-windows-msvc` : undefined;
const result = spawnSync('cargo', ['build', '--locked', '-p', 'subc-registration-native', ...(triple ? ['--target', triple] : [])], {cwd: root, stdio: 'inherit'});
if (result.status !== 0) process.exit(result.status ?? 1);
const target = process.env.CARGO_TARGET_DIR ?? resolve(root, 'target');
const library = process.platform === 'win32' ? 'subc_registration_native.dll'
  : process.platform === 'darwin' ? 'libsubc_registration_native.dylib' : 'libsubc_registration_native.so';
const destination = fileURLToPath(new URL('../native/', import.meta.url));
mkdirSync(destination, {recursive: true});
for (const file of ['index.js', 'index.d.ts', 'package.json']) {
  copyFileSync(resolve(root, 'clients/subc-registration-native', file), resolve(destination, file));
}
copyFileSync(resolve(target, ...(triple ? [triple] : []), 'debug', library), resolve(destination, `subc-registration-${process.platform}-${process.arch}.node`));
console.log(`Built bundle-local registration native addon: ${process.platform}-${process.arch}`);
