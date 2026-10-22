// Subscription usage alerts, guard side: the engine's `usage-alerts.json` is
// found (env, data dir, default), trusted only while fresh, and every Pi on a
// low account is told once and kept from starting new work, while other
// accounts stay usable. A quota-bearing API key (OpenCode Go) is one of those
// accounts. Run against the production `guard.mjs` (argv[2]) and
// `guard-preload.mjs` (argv[3]).
import assert from 'node:assert/strict';
import {mkdtempSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const preload = process.argv[3] ? await import(pathToFileURL(process.argv[3]).href + '?usage') : undefined;
const {usageAlertsPath, usageAlerts, providerOf, alertsOn, lowModelIn, usageBlock, usageNote, usageWatcher,
  NEW_WORK_TOOLS, USAGE_MARK} = guard;

const dir = mkdtempSync(join(tmpdir(), 'zeron-usage-alerts-'));
process.on('exit', () => rmSync(dir, {recursive: true, force: true}));
const file = join(dir, 'usage-alerts.json');
const env = {ZERON_USAGE_ALERTS_FILE: file};
const NOW = Date.parse('2026-10-18T00:00:00Z');
const alert = (provider, extra = {}) => ({
  key: `pi:${provider}:slot-${provider}:5h:${NOW / 1000 + 3600}`, harness: 'pi', provider, accountId: `slot-${provider}`,
  window: '5h', remainingPct: 4.5, resetsAt: new Date(NOW + 3600_000).toISOString(), ...extra,
});
let generation = 0;
const write = (alerts, {updatedAt = NOW, validForSecs = 900, version = 1} = {}) => {
  // A distinct size per write: the reader caches by mtime + size.
  writeFileSync(file, JSON.stringify({version, updatedAt: new Date(updatedAt).toISOString(), validForSecs,
    thresholdPct: 10, alerts, coverage: [], unsupported: ['pi:openai'], pad: 'x'.repeat(++generation)}));
};

// ── discovery: env, then the data dir, then ~/.zeron (standalone or env-less Pis)
assert.equal(usageAlertsPath(env), file);
assert.equal(usageAlertsPath({ZERON_DATA_DIR: '/d'}, '/h'), join('/d', 'usage-alerts.json'));
assert.equal(usageAlertsPath({}, '/h'), join('/h', '.zeron', 'usage-alerts.json'));
assert.deepEqual(usageAlerts({ZERON_USAGE_ALERTS_FILE: join(dir, 'missing.json')}, undefined, NOW), [], 'no file: nothing known');

// ── freshness, reset, shape
write([alert('anthropic'), {...alert('openai-codex'), resetsAt: new Date(NOW - 1).toISOString()}, {key: 'x', harness: 'codex'}]);
assert.deepEqual(usageAlerts(env, undefined, NOW).map((a) => a.provider), ['anthropic'], 'a reset window and foreign rows drop');
assert.deepEqual(usageAlerts(env, undefined, NOW + 901_000), [], 'a stale file (engine stopped) blocks nothing');
write([alert('anthropic')], {version: 2});
assert.deepEqual(usageAlerts(env, undefined, NOW), [], 'an unknown version is ignored');
writeFileSync(file, '{not json');
assert.deepEqual(usageAlerts(env, undefined, NOW), [], 'corrupt: nothing known');

// ── scoping helpers
assert.equal(providerOf('anthropic/claude-opus-5-5'), 'anthropic');
assert.equal(providerOf('clinepass/cline-pass/deepseek-v4.1-flash'), 'clinepass');
assert.equal(providerOf('claude-opus-5-5'), undefined, 'a bare id names no account');
const low = [alert('anthropic')];
assert.equal(alertsOn(low, 'Anthropic').length, 1);
assert.equal(alertsOn(low, 'openai-codex').length, 0);
assert.equal(lowModelIn({tasks: [{model: 'opencode-go/deepseek-v4.1-flash'}, {model: 'anthropic/claude-opus-5-5'}]}, low),
  'anthropic/claude-opus-5-5');
assert.equal(lowModelIn({requests: [{model: 'openai-codex/gpt-5.5'}]}, low), undefined);

// ── stop-new-work matrix: every spawning surface, own account low vs target account low
const opus = {provider: 'anthropic', id: 'claude-opus-5-5'};
const flash = {provider: 'opencode-go', id: 'deepseek-v4.1-flash'};
for (const toolName of ['subagent_start', 'workflow', 'delegate', 'zeron_create_chat', 'zeron_create_chats', 'advisor']) {
  assert.ok(NEW_WORK_TOOLS.has(toolName));
  const own = usageBlock({toolName, input: {}}, low, opus);
  assert.match(own, /Start no new work\. Finish the step you are on, write your handoff and end your turn\./, toolName);
  assert.ok(own.startsWith(USAGE_MARK));
  assert.equal(usageBlock({toolName, input: {model: 'opencode-go/deepseek-v4.1-flash'}}, low, flash), undefined,
    `${toolName}: an unaffected account stays usable`);
}
// An orchestrator on another provider is still kept from dispatching onto the low account.
const dispatch = usageBlock({toolName: 'zeron_create_chats', input: {requests: [{model: 'anthropic/claude-opus-5-5'}]}}, low, flash);
assert.match(dispatch, /anthropic\/claude-opus-5-5 runs on a subscription that is almost used up/);
assert.match(dispatch, /other accounts stay usable/);
assert.match(usageBlock({toolName: 'subagent_start', input: {tasks: [{task: 't', model: 'anthropic/x'}]}}, low, flash), /Blocked subagent_start/);
assert.equal(usageBlock({toolName: 'bash', input: {command: 'ls'}}, low, opus), undefined, 'the current step can finish');
assert.equal(usageBlock({toolName: 'edit', input: {}}, low, opus), undefined);
assert.equal(usageBlock({toolName: 'subagent_start', input: {}}, [], opus), undefined, 'no alert, no block');

// ── a quota-bearing API key (OpenCode Go) is a subscription like any other:
// the engine alerts on it by provider, so the same block matrix applies.
const goLow = [alert('opencode-go')];
assert.equal(providerOf('opencode-go/deepseek-v4.1-flash'), 'opencode-go');
assert.equal(alertsOn(goLow, 'opencode-go').length, 1);
assert.equal(alertsOn(goLow, 'anthropic').length, 0);
assert.match(
  usageBlock({toolName: 'subagent_start', input: {model: 'opencode-go/deepseek-v4.1-flash'}}, goLow, flash),
  /Start no new work\. Finish the step you are on, write your handoff and end your turn\./,
  'a low OpenCode Go key blocks new work on that key');
assert.match(usageBlock({toolName: 'subagent_start', input: {}}, goLow, flash), /Blocked subagent_start/,
  'a session already on the low key starts nothing new');
assert.equal(
  usageBlock({toolName: 'subagent_start', input: {model: 'anthropic/claude-opus-5-5'}}, goLow, opus),
  undefined, 'another account stays usable while an OpenCode Go key is low');
assert.match(
  usageBlock({toolName: 'subagent_start', input: {model: 'opencode-go/deepseek-v4.1-flash'}}, goLow, opus),
  /opencode-go\/deepseek-v4\.1-flash runs on a subscription that is almost used up/,
  'a session on another account is kept from dispatching onto the low key');
assert.equal(usageBlock({toolName: 'bash', input: {command: 'ls'}}, goLow, flash), undefined,
  'the current step can finish');
assert.match(usageNote(goLow, {now: NOW}), /usage-opencode-go-20261018T0000Z\.md/);

// ── notes: finish safely, handoff, end; helpers answer briefly
const note = usageNote(low, {now: NOW});
assert.match(note, /Finish the step you are on now/);
assert.match(note, /do not abandon a half-done action/);
assert.match(note, /\.pi\/handoffs\/usage-anthropic-20261018T0000Z\.md/);
assert.match(note, /end your turn/);
for (const surface of ['advisor', 'btw', 'decisions']) {
  const helper = usageNote(low, {surface});
  assert.match(helper, /Answer what you were asked as briefly as you can and stop/);
  assert.doesNotMatch(helper, /handoff/);
}

// ── delivery to running Pis: a fake Pi per surface
function fakePi() {
  const handlers = {};
  const sent = [];
  return {
    on: (name, fn) => { (handlers[name] ??= []).push(fn); },
    sendUserMessage: (text, opts) => sent.push({text, opts}),
    fire: (name, event, ctx) => (handlers[name] ?? []).map((fn) => fn(event, ctx)),
    sent,
  };
}
const ctx = (model, idle = false) => ({model, isIdle: () => idle});
let current = low;
const read = () => current;

// Sub-agent / delegate / workflow agent (depth > 0), already running: steered once.
for (const surface of ['subagent', 'workflow', 'delegate']) {
  const pi = fakePi();
  const w = usageWatcher(pi, {depth: 1, env: {PI_USAGE_SURFACE: surface}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(opus));
  assert.equal(w.check(), true, surface);
  assert.equal(pi.sent.length, 1);
  assert.equal(pi.sent[0].opts.deliverAs, 'steer');
  assert.match(pi.sent[0].text, /write a handoff/);
  assert.equal(w.check(), false, `${surface}: told once per key`);
  pi.fire('turn_start', {}, undefined);
  assert.equal(pi.sent.length, 1);
}
// Advisor (a helper Pi), already running: told to answer briefly.
{
  const pi = fakePi();
  const w = usageWatcher(pi, {depth: 1, env: {PI_USAGE_SURFACE: 'advisor'}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(opus));
  assert.equal(w.check(), true);
  assert.match(pi.sent[0].text, /as briefly as you can/);
}
// A child on another account is left alone.
{
  const pi = fakePi();
  const w = usageWatcher(pi, {depth: 1, env: {}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(flash));
  assert.equal(w.check(), false);
  assert.equal(pi.fire('before_agent_start', {}, ctx(flash))[0], undefined);
}
// A model switch onto the low account is noticed.
{
  const pi = fakePi();
  const w = usageWatcher(pi, {depth: 2, env: {}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(flash));
  pi.fire('model_select', {model: opus});
  assert.equal(w.check(), true);
}
// The Zeron session (depth 0) is steered by the engine; here it gets the note
// when its next turn starts, once. An idle child likewise.
for (const depth of [0, 1]) {
  const pi = fakePi();
  const w = usageWatcher(pi, {depth, env: {}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(opus, true));
  assert.equal(w.check(), false, 'idle or the session itself: no mid-turn steer');
  const [first] = pi.fire('before_agent_start', {}, ctx(opus, true));
  assert.equal(first.message.customType, 'zeron-usage-alert');
  assert.match(first.message.content, /write a handoff/);
  assert.equal(pi.fire('before_agent_start', {}, ctx(opus))[0], undefined, 'once per key');
  // A new window period is a new key: told again.
  current = [alert('anthropic', {key: 'next-period'})];
  assert.ok(pi.fire('before_agent_start', {}, ctx(opus))[0]);
  current = low;
}
// Cleared (reset or recovered): nothing more is said or blocked.
{
  current = [];
  const pi = fakePi();
  const w = usageWatcher(pi, {depth: 1, env: {}, intervalMs: 0, read});
  pi.fire('session_start', {}, ctx(opus));
  assert.equal(w.check(), false);
  current = low;
}

// ── children started without the parent's env find the snapshot (preload CONFIG)
if (preload) {
  const childEnv = {};
  const argv = ['node', '/x/cli.js'];
  preload.addGuard(argv, childEnv, () => true, {ext: '/g/guard.mjs', usage: '/data/usage-alerts.json'});
  assert.equal(childEnv.ZERON_USAGE_ALERTS_FILE, '/data/usage-alerts.json');
  const kept = {ZERON_USAGE_ALERTS_FILE: '/mine.json'};
  preload.addGuard(['node', '/x/cli.js'], kept, () => true, {ext: '/g/guard.mjs', usage: '/data/usage-alerts.json'});
  assert.equal(kept.ZERON_USAGE_ALERTS_FILE, '/mine.json', 'an inherited path wins');
}

console.log('pi usage alerts: ok');
