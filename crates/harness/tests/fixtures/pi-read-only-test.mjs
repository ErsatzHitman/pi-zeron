// The read-only guard's acceptance corpus: in a session created with
// `write: false`, what must be blocked and what must stay usable. Run against
// the production `guard.mjs` given as argv[2].
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const {classifyReadOnly, gitReadOnly, piDepth, readOnlyOn, shellWrites, subagentNote} = guard;

const failures = [];
const check = (label, got, expect) => {
  if (Boolean(got) !== expect) failures.push(`${expect ? 'not blocked' : 'blocked'}: ${label}${got ? ` (${got.id})` : ''}`);
};
const call = (toolName, input = {}, annotations) => classifyReadOnly({toolName, input, annotations});

const BLOCK_SHELL = [
  'echo hi > notes.md', 'cat a >> b', 'printf x>f', 'ls > out.txt 2>&1', 'tee out.txt', 'sed -i s/a/b/ f', 'mv a b', 'cp a b',
  'touch x', 'mkdir d', 'rm x', 'chmod +x f', 'python -c "open(1)"', 'node script.js', 'npm install', 'cargo build',
  'git commit -m x', 'git add .', 'git checkout -b x', 'git reset --hard', 'git push', 'git stash', 'git branch new-branch',
  'git tag v1', 'git config user.name x', 'git remote add o u', 'git -C repo commit -m x', 'git diff --output=p.patch',
  'find . -name x -delete', 'find . -exec touch {} \\;', 'sort -o out f', 'xargs touch', 'timeout 5 touch x', 'rtk git commit -m x',
  'ls; touch x', 'ls && mkdir d', 'cat f | tee g', 'echo $(touch x)', '$CMD', 'sh -c "ls"', 'bash script.sh', 'curl -o f u',
  'echo hi >> $HOME/x', 'cat <<EOF > f\nx\nEOF',
];
const ALLOW_SHELL = [
  'ls -la', 'cat README.md', 'rg -n "fn main" crates', 'grep -rn TODO src', 'head -50 f', 'tail -n 20 log', 'wc -l f',
  'find . -name "*.rs" -type f', 'git status', 'git log --oneline -20', 'git diff HEAD~1', 'git show HEAD:Cargo.toml',
  'git blame src/lib.rs', 'git branch -a', 'git branch --show-current', 'git tag -l', 'git remote -v', 'git stash list',
  'git worktree list', 'git config --get user.name', 'git -C repo log -1', 'git --no-pager log -1', 'rtk git status',
  'ls 2>/dev/null', 'rg x 2>&1 | head', 'cat f | sort | uniq -c', 'cd src && ls', 'jq .name package.json', 'tree -L 2',
  'du -sh .', 'which pi', 'echo "a > b"', "grep '->' f", 'pwd', 'diff a b', 'stat f', 'git rev-parse --show-toplevel',
  'timeout 10 rg x', 'xargs grep x', 'FOO=1 ls',
];
for (const command of BLOCK_SHELL) check(`bash: ${command}`, shellWrites(command), true);
for (const command of ALLOW_SHELL) check(`bash: ${command}`, shellWrites(command), false);
check('ps: Set-Content', shellWrites('Set-Content f x', 'ps'), true);
check('ps: Get-ChildItem', shellWrites('Get-ChildItem -Recurse', 'ps'), false);

// Tools: Pi's own, extension and MCP tools.
for (const [name, input] of [['write', {path: 'a', content: 'x'}], ['edit', {path: 'a'}], ['bash', {command: 'touch x'}],
  ['zeron_create_chat', {}], ['zeron_archive_chat', {}], ['zeron_respond_to_input', {}],
  ['mcp__fs__write_file', {}], ['apply_patch', {}], ['memory_save', {}], ['run_command', {}], ['git_commit', {}]])
  check(`tool: ${name}`, call(name, input), true);
for (const [name, input] of [['read', {path: 'a'}], ['grep', {}], ['find', {}], ['ls', {}], ['bash', {command: 'git status'}],
  ['zeron_list_chats', {}], ['zeron_read_chat', {}], ['zeron_show_visual', {}], ['web_search', {}], ['subagent', {}]])
  check(`tool: ${name}`, call(name, input), false);
check('tool: marked read-only', call('mcp__x__deploy', {}, {readOnlyHint: true}), false);
check('tool: marked destructive', call('mcp__x__thing', {}, {destructiveHint: true}), true);

assert.equal(gitReadOnly([]), true);
assert.equal(readOnlyOn({ZERON_READ_ONLY: '1'}), true);
assert.equal(readOnlyOn({}), false);

// Through Pi's hook: read-only blocks even with the delete toggle off, and
// the delete guard still works in a writing session.
const handlers = [];
guard.default({on: (event, handler) => handlers.push([event, handler]), getAllTools: () => []});
const hook = handlers[0][1];
const prior = {...process.env};
delete process.env.ZERON_DELETE_GUARD_FILE;
process.env.ZERON_DELETE_GUARD = '0';
process.env.ZERON_READ_ONLY = '1';
const blocked = hook({toolName: 'edit', input: {path: 'a'}}, {});
assert.equal(blocked?.block, true);
assert.match(blocked.reason, /\[zeron:read-only rule=ro-edit\]/);
assert.equal(hook({toolName: 'bash', input: {command: 'git status'}}, {}), undefined);
delete process.env.ZERON_READ_ONLY;
assert.equal(hook({toolName: 'edit', input: {path: 'a'}}, {}), undefined);
process.env.ZERON_DELETE_GUARD = '1';
assert.equal(hook({toolName: 'bash', input: {command: 'rm -rf /'}}, {})?.block, true);
for (const key of ['ZERON_DELETE_GUARD', 'ZERON_READ_ONLY', 'ZERON_DELETE_GUARD_FILE']) {
  if (prior[key] === undefined) delete process.env[key]; else process.env[key] = prior[key];
}

// A Pi that a Zeron session's Pi starts is told whose sub-agent it is.
const mcp = JSON.stringify({env: {ZERON_CHAT_ID: 'member-1'}});
const member = {ZERON_PI_MCP: mcp, ZERON_CHAT_TITLE: 'Fix the parser', ZERON_ROOT_CHAT_ID: 'orchestrator-r', ZERON_TREE_DEPTH: '2'};
assert.equal(subagentNote(member, 0), undefined, 'the session itself');
assert.match(subagentNote(member, 1), /sub-agent \(level 1\) of the agent in the Zeron session "Fix the parser" \(chat member-1\), at level 2 of the tree of orchestrator chat orchestrator-r\./);
assert.match(subagentNote({ZERON_PI_MCP: mcp}, 2), /\(level 2\) of the agent in the Zeron session chat member-1\. /);
assert.match(subagentNote(member, 1), /cannot use the Zeron board/);
assert.doesNotMatch(subagentNote(member, 1), /read-only/);
assert.match(subagentNote({...member, ZERON_READ_ONLY: '1'}, 1), /Your session is read-only, so you are too/);
assert.equal(subagentNote({}, 1), undefined, 'not under Zeron');
assert.equal(subagentNote({ZERON_PI_MCP: '{'}, 1), undefined);
assert.equal(piDepth({}, 7), 0, 'the Zeron session itself');
assert.equal(piDepth({ZERON_PI_PID: '7', ZERON_SUBAGENT_DEPTH: '0'}, 8), 1, 'its sub-agent');
assert.equal(piDepth({ZERON_PI_PID: '8', ZERON_SUBAGENT_DEPTH: '1'}, 8), 1, 'a reload keeps the level');
assert.equal(piDepth({ZERON_PI_PID: '8', ZERON_SUBAGENT_DEPTH: '1'}, 9), 2, "a sub-agent's sub-agent");
check('zeron_board_post', call('zeron_board_post', {kind: 'finding', text: 'x'}), false);
check('zeron_board_read', call('zeron_board_read'), false);
check('zeron_send_message', call('zeron_send_message', {chat: 'orchestrator-r', text: 'done'}), false);
check('zeron_create_chat', call('zeron_create_chat', {prompt: 'write it'}), true);
const keys = ['ZERON_SUBAGENT_DEPTH', 'ZERON_PI_PID', 'ZERON_PI_MCP'];
const priorSub = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
process.env.ZERON_PI_PID = String(process.pid + 1);   // started by another Pi
process.env.ZERON_SUBAGENT_DEPTH = '0';
process.env.ZERON_PI_MCP = mcp;
const child = [];
const load = () => guard.default({on: (event, handler) => child.push([event, handler]), getAllTools: () => []});
load();
assert.equal(process.env.ZERON_SUBAGENT_DEPTH, '1');
assert.equal(process.env.ZERON_PI_PID, String(process.pid));
load();
assert.equal(process.env.ZERON_SUBAGENT_DEPTH, '1', 'loading again in the same Pi keeps its level');
const start = child.find(([event]) => event === 'before_agent_start')?.[1];
assert.ok(start, 'a sub-agent gets the note');
const options = {sections: {preamble: 'BASE'}};
assert.equal(start({systemPrompt: 'BASE', systemPromptOptions: options}), undefined);
assert.match(options.sections.zeron_subagent, /^Zeron: you are a sub-agent \(level 1\)/);
assert.match(start({systemPrompt: 'BASE'}).systemPrompt, /^BASE\n\nZeron: you are a sub-agent \(level 1\)/, 'an older Pi');
assert.ok(!handlers.some(([event]) => event === 'before_agent_start'), 'the session itself gets none');
// /reload: the Zeron session gets the command, its sub-agents do not.
const commands = (load) => { const out = []; load({on: () => {}, getAllTools: () => [], registerCommand: (name, options) => out.push([name, options])}); return out; };
delete process.env.ZERON_PI_PID;
const session = commands(guard.default);
assert.deepEqual(session.map(([name]) => name), ['reload']);
let reloaded = 0;
await session[0][1].handler('', {reload: async () => { reloaded += 1; }});
assert.equal(reloaded, 1);
assert.deepEqual(commands(guard.default).map(([name]) => name), ['reload'], 'a reload keeps it');
process.env.ZERON_PI_PID = String(process.pid + 1);
assert.deepEqual(commands(guard.default), [], 'a sub-agent');
for (const [key, value] of Object.entries(priorSub)) {
  if (value === undefined) delete process.env[key]; else process.env[key] = value;
}

if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
console.log('read-only corpus passed');
