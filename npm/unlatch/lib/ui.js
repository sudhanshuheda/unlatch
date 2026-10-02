'use strict';
// Terminal output. In --json mode stdout carries exactly one JSON document and progress goes
// nowhere (agents parse stdout; stderr noise only confuses them).

class UsageError extends Error {}
class UserActionError extends Error {} // the user has to do something (exit 3)

function makeUi(sys, { json = false, quiet = false } = {}) {
  const color = !json && sys.stdoutIsTTY && !sys.env.NO_COLOR && sys.env.TERM !== 'dumb';
  const c = (code) => (s) => (color ? `\x1b[${code}m${s}\x1b[0m` : String(s));
  const ui = {
    json,
    bold: c('1'),
    dim: c('2'),
    green: c('32'),
    yellow: c('33'),
    red: c('31'),
    cyan: c('36'),
    /** Progress line (stderr; silent in --json). */
    step(msg) {
      if (!json && !quiet) sys.err(`${ui.dim('›')} ${msg}\n`);
    },
    ok(msg) {
      if (!json && !quiet) sys.err(`${ui.green('✓')} ${msg}\n`);
    },
    warn(msg) {
      if (!json) sys.err(`${ui.yellow('!')} ${msg}\n`);
    },
    /** Main human output (stdout). */
    print(msg = '') {
      if (!json) sys.out(msg + '\n');
    },
    emit(obj) {
      sys.out(JSON.stringify(obj, null, 2) + '\n');
    },
  };
  return ui;
}

module.exports = { makeUi, UsageError, UserActionError };
