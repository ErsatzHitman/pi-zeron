// The read-only guard's acceptance corpus: in a session created with
// `write: false`, what must be blocked and what must stay usable. Run against
// the production `guard.mjs` given as argv[2].
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const {classifyReadOnly, gitReadOnly, readOnlyOn, shellWrites} = guard;

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
  ['zeron_create_chat', {}], ['zeron_send_message', {}], ['zeron_archive_chat', {}], ['zeron_respond_to_input', {}],
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

if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
console.log('read-only corpus passed');
