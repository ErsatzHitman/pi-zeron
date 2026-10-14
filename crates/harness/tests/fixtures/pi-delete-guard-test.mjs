// The delete guard's acceptance corpus: what it must block and what it must
// leave alone, run against the production `guard.mjs` given as argv[2].
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';

const guard = await import(pathToFileURL(process.argv[2]).href);
const {classifyShell, classifyCall, decide, guardOn, nameTokens} = guard;

const b64 = text => Buffer.from(text, 'utf16le').toString('base64');
const failures = [];
const check = (label, got, expect) => {
  if (Boolean(got) !== expect) failures.push(`${expect ? 'not blocked' : 'blocked'}: ${label}${got ? ` (${got.id})` : ''}`);
};
const shell = (command, dialect = 'posix') => classifyShell(command, dialect);
const bash = command => decide({toolName: 'bash', input: {command}}, undefined, process.cwd());

const BLOCK = [
  'rm -rf x', 'sudo -u root rm x', '\\rm x', '/bin/rm x', 'r""m x', "r''m x", 'rm.exe x', 'xargs rm', 'unlink f', 'shred -u f',
  "find . -name '*.o' -delete", 'find . -exec rm {} \\;', 'echo a | xargs -I{} rm {}',
  "sh -c 'rm x'", 'bash -lc "cd d && rm x"', '$(rm x)', 'echo "`rm x`"', 'cat f; rmdir d', 'for x in a b; do rm $x; done',
  "echo 'rm x' | sh", 'echo cm0geA== | base64 -d | sh',
  'git clean -fdx', 'git rm f', 'git branch -D b', 'git -C d push origin :b', 'git stash drop',
  'rsync -a --delete a/ b/', 'mv f /dev/null', 'mkfs.ext4 /dev/sdb', 'dd if=/dev/zero of=/dev/sdb',
  'cmd /c del /f /q f', 'cmd //c "rd /s /q d"', 'C:\\Windows\\System32\\cmd.exe /c del x', 'robocopy a b /MIR',
  'powershell -Command "Remove-Item -Recurse d"', 'pwsh -c ri d', `powershell -enc ${b64('Remove-Item x')}`,
  `python -c "import os;os.remove('f')"`, "python - <<EOF\nimport shutil\nshutil.rmtree('d')\nEOF",
  `node -e "require('fs').rmSync('d',{recursive:true})"`, `ruby -e 'File.delete("f")'`, `perl -e 'unlink "f"'`,
  'python -m pip uninstall -y x',
  'rtk rm f', 'rtk proxy rm f', 'x=rm; $x f',
  'curl -X DELETE https://example.invalid/x', 'docker rm c', 'docker compose down', 'npm uninstall x', 'kubectl delete pod p',
  'psql -c "DROP TABLE t"', 'ssh h "rm x"', 'docker exec c rm -rf /d', 'wsl rm x',
  'pi --no-extensions -p x',
];
const BLOCK_PS = [
  'Remove-Item -Recurse d', 'Get-ChildItem | Remove-Item', 'gci | % { rm $_ }', 'foreach ($f in $l) { del $f }',
  'a && Remove-Item x', "[IO.File]::Delete('f')", '(Get-Item f).Delete()', '& "C:\\x\\rm.exe" a',
  'Set-Alias zap Remove-Item; zap x', 'Start-Process cmd -ArgumentList "/c del x"', 'Clear-RecycleBin -Force',
];
const SELF = ['node -e "console.log(process.env.ZERON_IPC_PORT)"', 'sed -i s/a/b/ harness-prefs.json', 'echo {} > harness-prefs.json',
  'echo {}>harness-prefs.json', 'bash -c "cat harness-prefs.json"'];
const ALLOW = [
  'ls', 'git status', 'git diff', 'cargo check', 'cargo test --workspace', 'echo hi 2>&1 | tee out.txt',
  'echo "rm -rf /"', 'grep -rn "rm -rf" .', 'rg Remove-Item', 'rg SetPiSettings', 'cat harness-prefs.json',
  'git commit -m "remove old files"', 'git log --diff-filter=D',
  'cat remove.txt', 'ls rm*', 'man rm', 'which rm', 'command -v rm',
  'git clean -n', 'git rm --cached f', 'git rm --help',
  "find . -name '*.tmp' -print", 'find . -exec grep -l foo {} +',
  'cat <<EOF\nrm -rf /\nEOF', 'git commit -F- <<EOF\nremove things\nEOF', 'docker run --rm img', 'docker ps',
  'npm run clean', 'make clean', 'git branch --show-current', 'node build.mjs', 'python script.py',
];
const ALLOW_PS = ['Get-Help Remove-Item', 'Get-Command Remove-Item', "Select-String -Pattern 'Remove-Item'", 'Get-ChildItem', 'Remove-Variable x'];

for (const c of BLOCK) check(c, bash(c), true);
for (const c of BLOCK_PS) check(`[ps] ${c}`, shell(c, 'ps'), true);
for (const c of SELF) check(`[self] ${c}`, bash(c), true);
for (const c of ALLOW) check(c, bash(c), false);
for (const c of ALLOW_PS) check(`[ps] ${c}`, decide({toolName: 'powershell', input: {command: c}}), false);

// Noise: quoting and escapes inside the command word change nothing.
for (const word of ['rm', 'Remove-Item']) {
  for (let i = 1; i < word.length; i++) {
    for (const noise of ["''", '""', '\\']) check(`[noise] ${word.slice(0, i)}${noise}${word.slice(i)} x`, shell(`${word.slice(0, i)}${noise}${word.slice(i)} x`), true);
    check(`[noise ps] ${word}`, shell(`${word.slice(0, i)}\`${word.slice(i)} x`, 'ps'), true);
  }
}
check('[noise cmd] d^el x', shell('d^el x', 'cmd'), true);
check('[noise cmd] r^d /s d', shell('r^d /s d', 'cmd'), true);

// Tool calls.
const sized = size => ({statSync: () => ({size})});
const missing = {statSync: () => { const e = new Error('missing'); e.code = 'ENOENT'; throw e; }};
check('empty write over a file', classifyCall({toolName: 'write', input: {path: 'a.txt', content: ''}, fs: sized(5)}), true);
check('empty write creating a file', classifyCall({toolName: 'write', input: {path: 'a.txt', content: ''}, fs: missing}), false);
check('write with content', classifyCall({toolName: 'write', input: {path: 'a.txt', content: 'x'}, fs: sized(5)}), false);
check('edit of the prefs file', decide({toolName: 'edit', input: {path: '/data/harness-prefs.json'}}), true);
check('write of the guard', decide({toolName: 'write', input: {path: '/d/pi-guard/zeron-delete-guard-0123456789abcdef.mjs', content: 'x'}}), true);
check('read of the prefs file', decide({toolName: 'read', input: {path: '/data/harness-prefs.json'}}), false);
check('a destructive tool', classifyCall({toolName: 'mcp__x__frobnicate', annotations: {destructiveHint: true}}), true);
check('a nested harmless bash', decide({toolName: 'bash', input: {command: 'ls'}, parentToolCallId: 'a/1'}), false);
check('an HTTP DELETE tool', classifyCall({toolName: 'http_request', input: {method: 'delete', url: 'u'}}), true);
for (const name of ['mcp__dokploy_kvm4__application_delete', 'mcp__dokploy_kvm4__docker_removeContainer', 'mcp__dokploy_kvm4__settings_cleanAll',
  'mcp__dokploy_kvm4__patch_markFileForDeletion', 'mcp__dokploy_kvm4__dockerDiskUsage_pruneBuildCache', 'mcp__github__delete_file',
  'mcp__claude_ai_Gmail__trash_message', 'memory_forget', 'mcp__dokploy_kvm4__dnsProvider_deleteRecord', 'mcp__dokploy_kvm4__deployment_removeDeployment'])
  check(name, classifyCall({toolName: name, input: {}}), true);
for (const name of ['mcp__dokploy_kvm4__settings_getLogCleanupStatus', 'mcp__dokploy_kvm4__settings_updateDockerCleanup', 'list_deleted_items',
  'get_deletion_status', 'mcp__claude_ai_Gmail__untrash_message', 'zeron_archive_chat', 'read', 'grep', 'subagent_start'])
  check(name, classifyCall({toolName: name, input: {}}), false);
assert.deepEqual(nameTokens('mcp__dokploy_kvm4__dockerDiskUsage_pruneBuildCache'), ['docker', 'disk', 'usage', 'prune', 'build', 'cache']);

// On or off: FORCE, then the live file, then the spawn snapshot, then on.
const stat = (mtimeMs, size = 10) => () => ({mtimeMs, size});
const read = text => () => text;
const fail = code => () => { const e = new Error(code); e.code = code; throw e; };
const env = {ZERON_DELETE_GUARD_FILE: '/p/harness-prefs.json', ZERON_DELETE_GUARD: '1'};
assert.equal(guardOn({ZERON_DELETE_GUARD_FORCE: '1', ...env}, {statSync: stat(1), readFileSync: read('{"blockDeletes":false}')}), true);
assert.equal(guardOn({}), true);
assert.equal(guardOn({ZERON_DELETE_GUARD: '0'}), false);
assert.equal(guardOn(env, {statSync: stat(1), readFileSync: read('{"blockDeletes":false}')}), false);
assert.equal(guardOn(env, {statSync: stat(1), readFileSync: fail('EIO')}), false, 'an unchanged file is not re-read');
assert.equal(guardOn(env, {statSync: stat(2), readFileSync: read('{}')}), true, 'a rewrite turns it back on');
assert.equal(guardOn(env, {statSync: stat(3), readFileSync: read('{bad')}), true, 'corrupt is on');
assert.equal(guardOn(env, {statSync: stat(4), readFileSync: read('{"blockDeletes":false}')}), false);
assert.equal(guardOn(env, {statSync: fail('EBUSY'), readFileSync: read('{}')}), false, 'a transient error keeps the last state');
assert.equal(guardOn(env, {statSync: stat(5), readFileSync: fail('EBUSY')}), false, 'a transient read error keeps the last state');
assert.equal(guardOn({...env, ZERON_DELETE_GUARD: '0'}, {statSync: fail('ENOENT'), readFileSync: read('{}')}), false, 'no file: the snapshot');
assert.equal(guardOn(env, {statSync: fail('ENOENT'), readFileSync: read('{}')}), true);

// The extension blocks through Pi's hook and stands aside when off.
const handlers = [];
guard.default({on: (event, handler) => handlers.push([event, handler]), getAllTools: () => [{name: 'nuke', annotations: {destructiveHint: true}}]});
assert.equal(handlers.length, 1);
assert.equal(handlers[0][0], 'tool_call');
const prior = process.env.ZERON_DELETE_GUARD_FILE;
delete process.env.ZERON_DELETE_GUARD_FILE;
process.env.ZERON_DELETE_GUARD = '1';
const blocked = handlers[0][1]({toolName: 'bash', input: {command: 'rm -rf /'}}, {cwd: process.cwd()});
assert.equal(blocked?.block, true);
assert.match(blocked.reason, /\[zeron:delete-guard rule=cmd-rm\]/);
assert.equal(handlers[0][1]({toolName: 'nuke', input: {}}, {})?.block, true);
assert.equal(handlers[0][1]({toolName: 'bash', input: {command: 'ls'}}, {}), undefined);
process.env.ZERON_DELETE_GUARD = '0';
assert.equal(handlers[0][1]({toolName: 'bash', input: {command: 'rm -rf /'}}, {}), undefined);
if (prior !== undefined) process.env.ZERON_DELETE_GUARD_FILE = prior;

if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
console.log('delete guard corpus passed');
