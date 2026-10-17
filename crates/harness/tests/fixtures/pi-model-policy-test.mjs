// Session model restrictions: the models a session disabled (its own file and
// its parents', named in ZERON_MODEL_POLICY) are kept from every Pi in it, and
// global helpers are left alone. Run against the production `guard.mjs` given
// as argv[2].
import assert from 'node:assert/strict';
import {mkdtempSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {delimiter, join} from 'node:path';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const {allowedFallback, disabledModelIn, disabledModels, idDisabled, modelDisabled, policyExempt} = guard;

const dir = mkdtempSync(join(tmpdir(), 'zeron-model-policy-'));
process.on('exit', () => rmSync(dir, {recursive: true, force: true}));
const own = join(dir, 'chat.json');
const parent = join(dir, 'root.json');
const missing = join(dir, 'gone.json');
writeFileSync(own, JSON.stringify({disabled: ['anthropic/claude-opus-4-5']}));
writeFileSync(parent, JSON.stringify({disabled: ['OpenAI/GPT-5.5', 7, '']}));
const env = {ZERON_MODEL_POLICY: [own, parent, missing].join(delimiter)};

// ── the lists: the chat's own and every parent's, case-folded
const off = disabledModels(env);
assert.deepEqual([...off].sort(), ['anthropic/claude-opus-4-5', 'openai/gpt-5.5']);
assert.equal(disabledModels({}).size, 0, 'no policy, nothing disabled');
console.log('policy: own and parent lists merge; a missing file disables nothing');

// ── edited mid-session: the next read sees it
writeFileSync(own, JSON.stringify({disabled: ['anthropic/claude-opus-4-5', 'anthropic/claude-haiku-4-5']}));
assert.ok(disabledModels(env).has('anthropic/claude-haiku-4-5'));
writeFileSync(own, '{"disabled": [');
assert.ok(disabledModels(env).has('anthropic/claude-haiku-4-5'), 'a torn write keeps the last list');
rmSync(own);
assert.ok(!disabledModels(env).has('anthropic/claude-opus-4-5'), 'a cleared list frees its models');
writeFileSync(own, JSON.stringify({disabled: ['anthropic/claude-opus-4-5']}));
console.log('policy: edits apply live');

// ── global helpers are not the session's
for (const surface of ['advisor', 'decisions', 'jev', 'memory', 'ext:uniichat']) {
  assert.ok(policyExempt({PI_USAGE_SURFACE: surface}), surface);
  assert.equal(disabledModels({...env, PI_USAGE_SURFACE: surface}).size, 0, surface);
}
for (const surface of [undefined, 'subagent', 'workflow']) {
  assert.ok(!policyExempt({PI_USAGE_SURFACE: surface}), String(surface));
}
console.log('policy: the advisor and other global helpers are exempt; sub-agents and workflows are not');

// ── naming a model
const now = disabledModels(env);
assert.ok(idDisabled(now, 'anthropic/claude-opus-4-5'));
assert.ok(idDisabled(now, 'Anthropic/Claude-Opus-4-5:high'), 'a thinking suffix names the same model');
assert.ok(idDisabled(now, 'claude-opus-4-5'), 'a bare id names it on any provider');
assert.ok(!idDisabled(now, 'openrouter/anthropic/claude-sonnet-4-5'));
assert.ok(!idDisabled(now, 'anthropic/claude-opus-4-5-mini'));
assert.ok(modelDisabled(now, {provider: 'openai', id: 'gpt-5.5'}));
assert.ok(!modelDisabled(now, {provider: 'openai', id: 'gpt-5.5-mini'}));
assert.ok(!modelDisabled(now, undefined));
assert.equal(disabledModelIn({tasks: [{prompt: 'x'}, {prompt: 'y', model: 'openai/gpt-5.5'}]}, now), 'openai/gpt-5.5');
assert.equal(disabledModelIn({models: ['x/y', 'anthropic/claude-opus-4-5']}, now), 'anthropic/claude-opus-4-5');
assert.equal(disabledModelIn({model: 'anthropic/claude-sonnet-4-5', prompt: 'openai/gpt-5.5'}, now), undefined,
  'only model fields count');
console.log('policy: tool inputs naming a disabled model are found');

// ── the fallback: an allowed model, same provider first
const opus = {provider: 'anthropic', id: 'claude-opus-4-5'};
const sonnet = {provider: 'anthropic', id: 'claude-sonnet-4-5'};
const gpt = {provider: 'openai', id: 'gpt-5.5'};
const mini = {provider: 'openai', id: 'gpt-5.5-mini'};
const ctx = (available, scoped = []) => ({modelRegistry: {getAvailable: () => available}, scopedModels: scoped});
assert.equal(allowedFallback(ctx([mini, opus, sonnet]), now, opus), sonnet);
assert.equal(allowedFallback(ctx([opus, gpt, mini]), now, opus), mini);
assert.equal(allowedFallback(ctx([opus, gpt]), now, opus), undefined);
assert.equal(allowedFallback(ctx([sonnet], [{model: mini}]), now, gpt), mini, 'scoped models come first');
console.log('policy: falls back to an allowed model of the same provider');

// ── the extension: every hook keeps the session off its disabled models
Object.assign(process.env, env);
delete process.env.PI_USAGE_SURFACE;
const handlers = {};
const switched = [];
const notes = [];
const pi = {
  on: (name, fn) => (handlers[name] ??= []).push(fn),
  setModel: async (model) => (switched.push(model), true),
  getAllTools: () => [],
  registerCommand: () => {},
};
guard.default(pi);
const ui = {notify: (message, type) => notes.push([type, message])};
const fire = async (name, event, context) => {
  let result;
  for (const fn of handlers[name] ?? []) result = (await fn(event, context)) ?? result;
  return result;
};
const session = {...ctx([opus, sonnet, gpt]), ui, model: opus};
await fire('session_start', {reason: 'startup'}, session);
assert.deepEqual(switched, [sonnet], 'a session starting on a disabled model moves off it');
await fire('model_select', {model: gpt, source: 'set'}, session);
assert.deepEqual(switched, [sonnet, sonnet], 'picking a disabled model is undone');
await fire('model_select', {model: sonnet, source: 'set'}, session);
assert.equal(switched.length, 2, 'an allowed pick stands');
let aborted = 0;
await fire('before_provider_request', {payload: {}}, {...session, model: gpt, abort: () => aborted++});
assert.equal(aborted, 1, 'a request on a disabled model is stopped');
await fire('before_provider_request', {payload: {}}, {...session, model: sonnet, abort: () => aborted++});
assert.equal(aborted, 1);
const blocked = await fire('tool_call', {toolName: 'subagent_start', input: {tasks: [{prompt: 'p', model: 'openai/gpt-5.5'}]}}, session);
assert.equal(blocked?.block, true);
assert.match(blocked.reason, /openai\/gpt-5\.5 is disabled in this session/);
const advisor = await fire('tool_call', {toolName: 'advisor', input: {model: 'openai/gpt-5.5'}}, session);
assert.notEqual(advisor?.block, true, "the advisor's model is its own");
assert.ok(notes.some(([type]) => type === 'warning'));
console.log('policy: session start, model picks, requests and tool calls are all held');

console.log('\nall model policy tests passed');
