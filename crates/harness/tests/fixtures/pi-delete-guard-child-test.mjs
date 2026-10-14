// The delete guard reaches every Pi a guarded Pi starts: the extension puts
// the preload in its node flags, and the preload hands each child Pi the
// extension. Run as `node <this> <guard.mjs> <guard-preload.mjs>`.
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync, readFileSync, realpathSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {pathToFileURL} from 'node:url';

const guardPath = resolve(process.argv[2]);
const preloadPath = resolve(process.argv[3]);
const {inheritIntoChildren} = await import(pathToFileURL(guardPath).href);
const {addGuard} = await import(pathToFileURL(preloadPath).href);
const preloadFlag = `--import=${pathToFileURL(preloadPath).href}`;

// addGuard: only the Pi script, once, fail closed.
const env = {ZERON_DELETE_GUARD_EXT: guardPath, ZERON_DELETE_GUARD_PI: '/pi/cli.js'};
let argv = ['node', '/pi/cli.js', '--mode', 'rpc'];
assert.equal(addGuard(argv, env, () => true), 'added');
assert.deepEqual(argv, ['node', '/pi/cli.js', '--extension', guardPath, '--mode', 'rpc']);
assert.equal(addGuard(argv, env, () => true), 'present');
assert.equal(argv.length, 6);
argv = ['node', '/pi/cli.js', '-e', guardPath];
assert.equal(addGuard(argv, env, () => true), 'present');
assert.equal(addGuard(['node', '/other/tool.js'], env, () => true), 'skip');
assert.equal(addGuard(['node', '/pi/cli.js'], {}, () => true, {}), 'skip');
assert.equal(addGuard(['node', '/pi/cli.js'], {ZERON_DELETE_GUARD_EXT: guardPath}, () => true, {}), 'added');
assert.equal(addGuard(['node', '/pi/other.js'], {ZERON_DELETE_GUARD_EXT: guardPath}, () => true, {}), 'skip');
assert.equal(addGuard(['node', '/pi/cli.js'], env, () => false), 'missing');
// No env (a herdr pane, a new terminal tab): the written-in config stands in
// and is restored for the child's own children.
const bare = {};
const config = {ext: guardPath, file: '/p/harness-prefs.json', force: true};
argv = ['node', '/src/cli.ts', '--mode', 'rpc'];
assert.equal(addGuard(argv, bare, () => true, config, '/d/preload.mjs'), 'added');
assert.deepEqual(argv.slice(2, 4), ['--extension', guardPath]);
assert.deepEqual(bare, {
  ZERON_DELETE_GUARD_EXT: guardPath,
  ZERON_DELETE_GUARD_FILE: '/p/harness-prefs.json',
  ZERON_DELETE_GUARD_FORCE: '1',
  ZERON_DELETE_GUARD_PRELOAD: '/d/preload.mjs',
});

// inheritIntoChildren: the preload joins the node flags once, and names this
// Pi's script for its children.
const proc = {
  argv: ['node', '/pi/cli.js'],
  execArgv: ['--import', 'file:///resolver.ts'],
  env: {ZERON_DELETE_GUARD_EXT: guardPath, ZERON_DELETE_GUARD_PRELOAD: preloadPath},
};
assert.equal(inheritIntoChildren(proc), true);
assert.equal(inheritIntoChildren(proc), true);
assert.deepEqual(proc.execArgv, ['--import', 'file:///resolver.ts', preloadFlag]);
assert.equal(proc.env.ZERON_DELETE_GUARD_PI, resolve('/pi/cli.js'));
assert.equal(inheritIntoChildren({argv: [], execArgv: [], env: {}}), false);

// End to end: a child started the way Pi's launchers start one.
const dir = mkdtempSync(join(tmpdir(), 'zeron-guard-child-'));
const fakePi = join(dir, 'pi.mjs');
writeFileSync(fakePi, 'console.log(JSON.stringify({argv: process.argv.slice(2), execArgv: process.execArgv}));\n');
const other = join(dir, 'other.mjs');
writeFileSync(other, 'console.log(JSON.stringify({argv: process.argv.slice(2)}));\n');
const childEnv = {
  ...process.env,
  ZERON_DELETE_GUARD_EXT: guardPath,
  ZERON_DELETE_GUARD_PI: fakePi,
};
const run = (script, extraEnv = {}) =>
  spawnSync(process.execPath, [preloadFlag, script, '--mode', 'json'], {
    env: {...childEnv, ...extraEnv},
    encoding: 'utf8',
  });
const child = run(fakePi);
assert.equal(child.status, 0, child.stderr);
const seen = JSON.parse(child.stdout);
assert.deepEqual(seen.argv, ['--extension', guardPath, '--mode', 'json']);
// Its own children inherit the preload through the same flags.
assert.ok(seen.execArgv.includes(preloadFlag), JSON.stringify(seen.execArgv));
const untouched = run(other);
assert.equal(untouched.status, 0, untouched.stderr);
assert.deepEqual(JSON.parse(untouched.stdout).argv, ['--mode', 'json']);
const missing = run(fakePi, {ZERON_DELETE_GUARD_EXT: join(dir, 'gone.mjs')});
assert.equal(missing.status, 1);
assert.match(missing.stderr, /delete guard extension is missing/);

// A Pi typed into a herdr pane: the parent's node flags, none of its env.
const configured = join(dir, 'configured-preload.mjs');
const marker = 'export const CONFIG = {}; // zeron:guard-config';
const source = readFileSync(preloadPath, 'utf8');
assert.ok(source.includes(marker));
writeFileSync(configured, source.replace(marker, `export const CONFIG = ${JSON.stringify({ext: guardPath, file: join(dir, 'prefs.json'), force: false})}; // zeron:guard-config`));
const cli = join(dir, 'cli.mjs');
writeFileSync(cli, 'console.log(JSON.stringify({argv: process.argv.slice(2), ext: process.env.ZERON_DELETE_GUARD_EXT, file: process.env.ZERON_DELETE_GUARD_FILE, preload: process.env.ZERON_DELETE_GUARD_PRELOAD}));\n');
const cleanEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('ZERON_')));
const pane = spawnSync(process.execPath, [`--import=${pathToFileURL(configured).href}`, cli, '--mode', 'json'], {env: cleanEnv, encoding: 'utf8'});
assert.equal(pane.status, 0, pane.stderr);
const paneSeen = JSON.parse(pane.stdout);
assert.deepEqual(paneSeen.argv, ['--extension', guardPath, '--mode', 'json']);
assert.equal(paneSeen.ext, guardPath);
assert.equal(paneSeen.file, join(dir, 'prefs.json'));
// macOS temp is a symlink (/var -> /private/var).
assert.equal(realpathSync(paneSeen.preload), realpathSync(configured));

console.log('delete guard child coverage passed');
