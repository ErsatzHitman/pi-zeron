// Orchestrator tree policy: builds and tests on GitHub only, a limit on heavy
// commands at once, the models a tree may use, and the halt the spending cap
// sets. Every Pi of the tree reads the file named in ZERON_TREE_POLICY. Run
// against the production `guard.mjs` given as argv[2].
import assert from 'node:assert/strict';
import {existsSync, mkdtempSync, readdirSync, rmSync, utimesSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const {
  heavyCommand, modelMatches, modelNotAllowed, disallowedModelIn, takeSlot, dropSlot, treeBlock, treePolicy, shellCommandOf,
} = guard;

const dir = mkdtempSync(join(tmpdir(), 'zeron-tree-policy-'));
process.on('exit', () => rmSync(dir, {recursive: true, force: true}));
const file = join(dir, 'root.json');
const write = (policy) => writeFileSync(file, JSON.stringify({rootId: 'orchestrator-r', ...policy}));
const env = {ZERON_TREE_POLICY: file};
const bare = {githubOnlyTests: false, heavyCommandLimit: 0, allowedModels: [], halted: undefined};

// ── the file: read live, unknown blocks nothing
assert.equal(treePolicy({}), undefined, 'no env, no policy');
assert.equal(treePolicy(env), undefined, 'a missing file, no policy');
write({githubOnlyTests: true, heavyCommandLimit: 2, allowedModels: ['openai/gpt-5.6', ' ', 7]});
assert.deepEqual(treePolicy(env), {
  githubOnlyTests: true, heavyCommandLimit: 2, allowedModels: ['openai/gpt-5.6'], halted: undefined,
});
write({githubOnlyTests: true, heavyCommandLimit: 0, halted: {reason: 'the spending cap of $5.00 is reached'}});
const halted = treePolicy(env);
assert.equal(halted.heavyCommandLimit, 0);
assert.equal(halted.halted.reason, 'the spending cap of $5.00 is reached');
writeFileSync(file, '{"githubOnlyTests": ');
assert.equal(treePolicy(env), undefined, 'a torn file blocks nothing');
write({githubOnlyTests: true});
assert.equal(treePolicy(env).githubOnlyTests, true, 'an edit applies at once');
for (const surface of ['advisor', 'decisions', 'jev', 'memory']) {
  assert.equal(treePolicy({...env, PI_USAGE_SURFACE: surface}), undefined, `${surface} is a global helper`);
}
console.log('tree policy: the file is read live; unknown blocks nothing; global helpers are exempt');

// ── which commands are heavy
const HEAVY = [
  'cargo build', 'cargo test -p zeron-engine', 'cargo +nightly clippy --all-targets', 'cargo check --workspace', 'cargo nextest run',
  'rtk cargo test', 'timeout 600 cargo build --release', 'env RUSTFLAGS=-Dwarnings cargo check', 'nohup cargo run &',
  'cd crates/ui && cargo test', 'npm test', 'npm run build', 'npm run test:unit', 'pnpm build', 'pnpm --filter web test', 'yarn lint',
  'bun run typecheck', 'npx vitest run', 'npx tsc --noEmit', 'pnpm dlx jest', 'pytest -x', 'python -m pytest tests', 'python3 -m unittest',
  'go test ./...', 'go build ./cmd/x', 'dotnet build', 'dotnet test', 'gradle build', './gradlew test', 'mvn package', 'make', 'make -j8 all',
  'cmake --build .', 'ninja', 'msbuild app.sln', 'xcodebuild test', 'tsc -p .', 'rustc main.rs', 'bash -c "cargo test"',
  'sh -c \'cd x && cargo build\'', 'echo $(cargo build)', 'git pull && cargo test', 'ls | xargs cargo check', 'FOO=1 cargo test',
  'powershell -Command "cargo test"',
];
const LIGHT = [
  'cargo fmt', 'cargo fmt --check', 'cargo metadata', 'cargo --version', 'cargo tree', 'rustfmt --edition 2024 src/lib.rs',
  'npm install', 'npm ci', 'npm run dev', 'npm view react', 'pnpm install', 'yarn add left-pad', 'node script.js', 'python script.py',
  'git status', 'git commit -m "cargo test is slow"', 'git push -u origin x', 'gh pr checks', 'gh run watch 123', 'gh run view --log-failed',
  'ls -la', 'rg "cargo build" docs', 'grep -rn "npm test" .', 'echo cargo test', 'cat Makefile', 'which make', 'sed -n 1,5p Cargo.toml',
  'cd crates && ls', 'go version', 'go mod tidy', 'docker ps', 'curl -s https://example.com',
];
for (const command of HEAVY) assert.ok(heavyCommand(command), `heavy: ${command}`);
for (const command of LIGHT) assert.equal(heavyCommand(command), undefined, `light: ${command}`);
assert.ok(heavyCommand('cargo test', 'ps'), 'PowerShell runs it too');
assert.ok(heavyCommand('& cargo build', 'ps'));
assert.equal(heavyCommand('cargo test').name, 'cargo test');
assert.equal(heavyCommand('pnpm run build:web').name, 'pnpm run build:web');
assert.equal(heavyCommand(undefined), undefined);
assert.equal(heavyCommand(`echo ${'x'.repeat(300000)}`), undefined, 'too large to analyse blocks nothing');
console.log('tree policy: compilers, bundlers and test runners are heavy; fmt, git, gh and searches are not');

// ── which tool calls run a shell line
assert.deepEqual(shellCommandOf({toolName: 'bash', input: {command: 'ls'}}), {command: 'ls', dialect: 'posix'});
assert.equal(shellCommandOf({toolName: 'powershell', input: {command: 'ls'}}).dialect, 'ps');
assert.equal(shellCommandOf({toolName: 'bash_bg', input: {command: 'cargo test'}}).command, 'cargo test');
assert.equal(shellCommandOf({toolName: 'read', input: {command: 'cargo test'}}), undefined, 'only shells run commands');
assert.equal(shellCommandOf({toolName: 'bash', input: {}}), undefined);
console.log('tree policy: shell-like tools are found by name and input');

// ── github-only tests
const tree = (policy) => ({...bare, ...policy});
const gh = tree({githubOnlyTests: true});
const run = (command, policy = gh, toolName = 'bash') => treeBlock({toolName, input: {command}}, policy);
for (const command of ['cargo test', 'rtk cargo build', 'npm run build', 'pytest']) {
  const reason = run(command);
  assert.match(reason, /^\[zeron:tree-policy rule=github-only-tests\]/, command);
  assert.match(reason, /gh pr checks/);
  assert.match(reason, /Do not retry or rephrase/);
}
for (const command of ['cargo fmt', 'git push', 'gh pr checks', 'rg test', 'npm install']) {
  assert.equal(run(command), undefined, command);
}
assert.equal(run('cargo test', tree({})), undefined, 'off, nothing is held');
assert.equal(run('cargo test', gh, 'read'), undefined, 'a non-shell tool is not a shell');
assert.match(run('cargo build', gh, 'bash_bg'), /github-only-tests/);
assert.equal(treeBlock({toolName: 'bash', input: {command: 'cargo test'}}, undefined), undefined);
console.log('tree policy: github-only tests hold builds and tests off this machine and point at CI');

// ── models
const allowed = ['openai/gpt-5.6', 'claude-sonnet-4-5'];
assert.ok(modelMatches('openai/gpt-5.6', 'openai/gpt-5.6'));
assert.ok(modelMatches('openai/gpt-5.6', 'gpt-5.6'), 'a bare id names the model on any provider');
assert.ok(modelMatches('claude-sonnet-4-5', 'anthropic/claude-sonnet-4-5'));
assert.ok(modelMatches('OpenAI/GPT-5.6:high', 'openai/gpt-5.6'), 'case and a thinking suffix are ignored');
assert.ok(!modelMatches('openai/gpt-5.6', 'azure/gpt-5.6'), 'another provider is another model');
assert.ok(!modelMatches('openai/gpt-5.6', 'openai/gpt-5.6-mini'));
assert.ok(!modelNotAllowed([], 'anything/at-all'), 'an empty list allows any');
assert.ok(!modelNotAllowed(allowed, 'gpt-5.6'));
assert.ok(modelNotAllowed(allowed, 'anthropic/claude-opus-4-5'));
assert.ok(!modelNotAllowed(allowed, undefined) && !modelNotAllowed(allowed, ''), 'no model named, nothing to refuse');
assert.equal(disallowedModelIn({tasks: [{model: 'gpt-5.6'}, {model: 'xai/grok-5'}]}, allowed), 'xai/grok-5');
assert.equal(disallowedModelIn({model: 'openai/gpt-5.6', prompt: 'use xai/grok-5'}, allowed), undefined, 'only model fields count');
assert.equal(disallowedModelIn({model: 'xai/grok-5'}, []), undefined);
const models = tree({allowedModels: allowed});
const create = (model) => treeBlock({toolName: 'zeron_create_chat', input: {model}}, models);
assert.match(create('xai/grok-5'), /^\[zeron:tree-policy rule=allowed-models\]/);
assert.match(create('xai/grok-5'), /openai\/gpt-5\.6, claude-sonnet-4-5/);
assert.equal(create('gpt-5.6'), undefined);
assert.equal(treeBlock({toolName: 'advisor', input: {model: 'xai/grok-5'}}, models), undefined, "the advisor's model is its own");
console.log('tree policy: models outside the allow-list are refused; bare ids match any provider');

// ── a halted tree starts nothing and changes nothing
const stopped = tree({halted: {reason: 'the spending cap of $5.00 is reached'}});
const halt = (toolName, input = {}) => treeBlock({toolName, input}, stopped);
for (const [name, input] of [
  ['subagent_start', {}], ['zeron_create_chat', {}], ['workflow', {}], ['delegate', {}], ['write', {path: 'a'}], ['edit', {path: 'a'}],
  ['bash', {command: 'touch x'}], ['bash', {command: 'git commit -m x'}], ['zeron_send_message', {}],
]) {
  const reason = halt(name, input);
  assert.match(reason, /^\[zeron:tree-policy rule=halted\]/, name);
  assert.match(reason, /spending cap of \$5\.00 is reached/);
}
for (const [name, input] of [['read', {path: 'a'}], ['grep', {}], ['bash', {command: 'git status'}], ['bash', {command: 'rg x'}], ['zeron_read_chat', {}]]) {
  assert.equal(halt(name, input), undefined, name);
}
console.log('tree policy: a halted tree may read and nothing else');

// ── heavy slots: shared by every Pi of the tree through files
const slots = join(dir, 'root.slots');
const live = () => true;
const a = takeSlot(slots, 2, 'a', {pid: 111, alive: live});
const b = takeSlot(slots, 2, 'b', {pid: 222, alive: live});
assert.ok(a && b);
assert.equal(takeSlot(slots, 2, 'c', {pid: 333, alive: live}), undefined, 'the third waits');
dropSlot(a);
const c = takeSlot(slots, 2, 'c', {pid: 333, alive: live});
assert.ok(c, 'a freed slot is taken');
assert.equal(takeSlot(slots, 2, 'd', {pid: 444, alive: live}), undefined);
// A dead process holds nothing.
const reaped = takeSlot(slots, 2, 'e', {pid: 555, alive: (pid) => pid !== 222});
assert.ok(reaped, "a dead Pi's slot is reaped");
assert.ok(!existsSync(b));
// So does an old one.
dropSlot(c);
dropSlot(reaped);
const old = takeSlot(slots, 1, 'old', {pid: 666, alive: live});
assert.ok(old);
assert.equal(takeSlot(slots, 1, 'early', {pid: 888, alive: live}), undefined, 'a live slot holds');
const longAgo = new Date(Date.now() - 3 * 60 * 60 * 1000);
utimesSync(old, longAgo, longAgo);
assert.ok(takeSlot(slots, 1, 'fresh', {pid: 777, alive: live}), 'a stale slot is reaped');
dropSlot(old);
for (const name of readdirSync(slots)) rmSync(join(slots, name));
console.log('tree policy: heavy slots are counted across processes and reaped when stale or dead');

// ── the extension: every hook applies the policy
process.env.ZERON_TREE_POLICY = file;
process.env.ZERON_TREE_SLOT_WAIT_MS = '1200';
delete process.env.PI_USAGE_SURFACE;
delete process.env.ZERON_MODEL_POLICY;
const handlers = {};
const pi = {
  on: (name, fn) => (handlers[name] ??= []).push(fn),
  setModel: async () => true,
  getAllTools: () => [],
  registerCommand: () => {},
};
guard.default(pi);
const fire = async (name, event, context = {}) => {
  let result;
  for (const fn of handlers[name] ?? []) result = (await fn(event, context)) ?? result;
  return result;
};
const call = (id, command) => fire('tool_call', {toolName: 'bash', toolCallId: id, input: {command}});

write({githubOnlyTests: true});
const refused = await call('t0', 'cargo test');
assert.equal(refused?.block, true);
assert.match(refused.reason, /github-only-tests/);
assert.notEqual((await call('t0', 'git status'))?.block, true);
write({heavyCommandLimit: 1});
assert.notEqual((await call('h1', 'cargo build'))?.block, true, 'the first heavy command runs');
assert.notEqual((await call('l1', 'cargo fmt'))?.block, true, 'a light command is never held');
const started = Date.now();
const over = await call('h2', 'cargo test');
assert.equal(over?.block, true, 'the second waits, then is refused');
assert.ok(Date.now() - started >= 1000, 'it waited for a slot first');
assert.match(over.reason, /heavy-command-limit/);
assert.match(over.reason, /allows 1 at once/);
// Finishing the first frees its slot.
await fire('tool_result', {toolCallId: 'h1', toolName: 'bash'});
assert.notEqual((await call('h3', 'cargo test'))?.block, true, 'a finished command frees its slot');
await fire('agent_end', {});
assert.notEqual((await call('h4', 'cargo check'))?.block, true, 'a finished turn frees every slot it held');
await fire('agent_end', {});
write({halted: {reason: 'the spending cap is reached'}});
const edit = await fire('tool_call', {toolName: 'write', toolCallId: 'w1', input: {path: 'a', content: 'x'}});
assert.equal(edit?.block, true);
assert.match(edit.reason, /rule=halted/);
let aborted = 0;
const notes = [];
await fire('before_provider_request', {payload: {}}, {
  model: {provider: 'openai', id: 'gpt-5.6'}, abort: () => aborted++, ui: {notify: (message) => notes.push(message)},
});
assert.equal(aborted, 1, 'a halted tree sends no further model request');
assert.match(notes[0], /spending cap is reached/);
write({allowedModels: ['openai/gpt-5.6']});
await fire('before_provider_request', {payload: {}}, {model: {provider: 'openai', id: 'gpt-5.6'}, abort: () => aborted++});
assert.equal(aborted, 1, 'an allowed model is left alone');
await fire('before_provider_request', {payload: {}}, {
  model: {provider: 'xai', id: 'grok-5'}, abort: () => aborted++, ui: {notify: () => {}},
});
assert.equal(aborted, 2, 'a request on a model outside the list is stopped');
const create2 = await fire('tool_call', {toolName: 'subagent_start', toolCallId: 's1', input: {tasks: [{model: 'xai/grok-5'}]}});
assert.equal(create2?.block, true);
rmSync(file);
assert.notEqual((await call('x1', 'cargo test'))?.block, true, 'with the file gone nothing is held');
console.log('tree policy: session hooks hold the tree to its policy and release heavy slots');

console.log('\nall tree policy tests passed');
