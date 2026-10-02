#!/usr/bin/env node
'use strict';
// `npx unlatch` entry point. Zero dependencies: everything is in ../lib.

const major = Number(process.versions.node.split('.')[0]);
if (major < 18) {
  process.stderr.write(`unlatch needs Node.js 18 or later (this is ${process.version}).\n`);
  process.exit(1);
}

const { main } = require('../lib/cli');
const { realSys } = require('../lib/sys');

main(process.argv.slice(2), realSys()).then(
  (code) => process.exit(code),
  (e) => {
    process.stderr.write(`unlatch: ${(e && e.stack) || e}\n`);
    process.exit(1);
  }
);
