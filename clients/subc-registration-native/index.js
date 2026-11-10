'use strict';
// Loaded code is selected only from this package's directory, never from PATH or
// an environment override. Emulated runtimes select their own architecture.
if (!['linux', 'darwin', 'win32'].includes(process.platform) ||
    !['x64', 'arm64'].includes(process.arch)) {
  throw new Error('registration native platform unsupported');
}
module.exports = require(`./subc-registration-${process.platform}-${process.arch}.node`);
