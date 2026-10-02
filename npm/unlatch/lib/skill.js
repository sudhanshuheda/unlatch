'use strict';
// `unlatch skill [--claude|--codex|--all] [--print]`: teach the local coding agent how to share
// a folder. skill/SKILL.md is Generated from site/src/SKILL.md by site/build.sh (the same text
// the site serves as /SKILL.md); test/skill.test.js fails if they drift.
//   Claude Code: ~/.claude/skills/<name>/SKILL.md
//   Codex CLI:   ~/.agents/skills/<name>/SKILL.md (current docs); also ~/.codex/skills/<name>/
//                when that older directory already exists.

const path = require('node:path');
const names = require('./names');

function skillText(sys) {
  return sys.fs.readFileSync(path.join(__dirname, '..', 'skill', 'SKILL.md'), 'utf8');
}

function isDir(sys, p) {
  try {
    return sys.fs.statSync(p).isDirectory();
  } catch {
    return false;
  }
}

/** Which agents to install for: explicit flags, else the ones that look installed, else both. */
function targets(sys, opts) {
  let claude = !!(opts.claude || opts.all);
  let codex = !!(opts.codex || opts.all);
  if (!claude && !codex) {
    claude = isDir(sys, path.join(sys.home, '.claude'));
    codex = isDir(sys, path.join(sys.home, '.codex')) || isDir(sys, path.join(sys.home, '.agents'));
    if (!claude && !codex) claude = codex = true;
  }
  const out = [];
  if (claude) out.push({ agent: 'claude-code', dir: path.join(sys.home, '.claude', 'skills', names.SKILL_NAME) });
  if (codex) {
    out.push({ agent: 'codex', dir: path.join(sys.home, '.agents', 'skills', names.SKILL_NAME) });
    if (isDir(sys, path.join(sys.home, '.codex', 'skills'))) {
      out.push({ agent: 'codex (legacy dir)', dir: path.join(sys.home, '.codex', 'skills', names.SKILL_NAME) });
    }
  }
  return out;
}

function installSkill(sys, ui, opts) {
  const text = skillText(sys);
  if (opts.print) {
    sys.out(text);
    return { ok: true, printed: true };
  }
  const written = [];
  for (const t of targets(sys, opts)) {
    sys.fs.mkdirSync(t.dir, { recursive: true });
    const file = path.join(t.dir, 'SKILL.md');
    sys.fs.writeFileSync(file, text);
    written.push({ agent: t.agent, path: file });
  }
  const res = { ok: true, installed: written };
  if (ui.json) ui.emit(res);
  else {
    for (const w of written) ui.ok(`${w.agent}: ${w.path}`);
    ui.print(ui.dim(`Now ask your agent to "show this folder in Finder"; it will run \`${names.npx('share')}\` and hand you the Mac command.`));
  }
  return res;
}

module.exports = { installSkill, skillText, targets };
