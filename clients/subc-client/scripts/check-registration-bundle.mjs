import { accessSync } from 'node:fs';
for (const platform of ['linux', 'darwin', 'win32']) {
  for (const arch of ['x64', 'arm64']) {
    accessSync(new URL(`../native/subc-registration-${platform}-${arch}.node`, import.meta.url));
  }
}
console.log('Verified all six bundle-local registration native artifacts are present');
