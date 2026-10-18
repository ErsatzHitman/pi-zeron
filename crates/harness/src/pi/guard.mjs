// Zeron delete guard. Blocks tool calls that delete files, git refs or remote
// resources, in every Pi run Zeron starts and every child that inherits it.
// In a read-only session (`ZERON_READ_ONLY=1`, a delegated chat created with
// `write: false`) it also blocks every edit, whatever the delete toggle says.
// A guard against mistakes and model-driven changes, not a sandbox.
import { existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { createRequire, syncBuiltinESMExports } from "node:module";
import { homedir } from "node:os";
import { basename, delimiter, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

// ---- on/off: FORCE > live file > spawn snapshot > ON
let seen = { file: "", mtimeMs: -1, size: -1, on: undefined };
export function guardOn(env = process.env, fs = { statSync, readFileSync }) {
  if (env.ZERON_DELETE_GUARD_FORCE === "1") return true;
  const file = env.ZERON_DELETE_GUARD_FILE;
  if (!file) return env.ZERON_DELETE_GUARD !== "0";
  const last = () => (seen.file === file && seen.on !== undefined ? seen.on : true);
  let st;
  try { st = fs.statSync(file); }
  catch (e) { return e?.code === "ENOENT" ? env.ZERON_DELETE_GUARD !== "0" : last(); }
  if (file === seen.file && st.mtimeMs === seen.mtimeMs && st.size === seen.size) return seen.on;
  try {
    const prefs = JSON.parse(fs.readFileSync(file, "utf8"));
    seen = { file, mtimeMs: st.mtimeMs, size: st.size, on: prefs?.blockDeletes !== false };
    return seen.on;
  } catch (e) {
    // Corrupt: ON, as the engine reads it. Transient I/O (a Windows
    // rename-over): keep the last state.
    if (e instanceof SyntaxError) { seen = { file, mtimeMs: st.mtimeMs, size: st.size, on: true }; return true; }
    return last();
  }
}

// ---- tokenizer: three dialects, raw+cooked words, nested $(...) / backticks, heredocs
export function split(src, dialect /* "posix" | "ps" | "cmd" */) {
  const ps = dialect === "ps", cmd = dialect === "cmd";
  const esc = ps ? "`" : cmd ? "^" : "\\";
  const segs = [], nested = [], heredocs = [];
  let words = [], raw = "", cooked = "", open = false, dyn = false, pending = [];
  const word = () => { if (open) words.push({ raw, cooked, dyn }); raw = cooked = ""; open = dyn = false; };
  const seg = () => { word(); if (words.length) segs.push(words); words = []; };
  const paren = (from) => {                       // index just after the ")" matching an opened "("
    let depth = 1, j = from;
    for (; j < src.length && depth; j++) depth += src[j] === "(" ? 1 : src[j] === ")" ? -1 : 0;
    nested.push({ src: src.slice(from, depth ? j : j - 1), dialect });
    return j;
  };
  const tick = (from) => {                        // posix backtick substitution; returns index of closing "`"
    const e = src.indexOf("`", from + 1), end = e < 0 ? src.length : e;
    nested.push({ src: src.slice(from + 1, end), dialect });
    return end;
  };
  for (let i = 0; i < src.length; i++) {
    const c = src[i];
    if (c === esc && i + 1 < src.length) {         // \x  `x  ^x   (posix \<newline> = continuation)
      if (!ps && !cmd && src[i + 1] === "\n") { i++; continue; }
      raw += c + src[i + 1]; cooked += src[i + 1]; open = true; i++; continue;
    }
    if (c === "'" && !cmd) {                       // literal; PowerShell doubles '' for a quote
      let j = i + 1, text = "";
      for (; j < src.length; j++) {
        if (src[j] === "'") { if (ps && src[j + 1] === "'") { text += "'"; j++; continue; } break; }
        text += src[j];
      }
      raw += src.slice(i, j + 1); cooked += text; open = true; i = j; continue;
    }
    if (c === '"') {                               // expandable; $() and backticks inside recurse
      let j = i + 1, text = "";
      for (; j < src.length && src[j] !== '"'; j++) {
        if (src[j] === esc && !cmd) { text += src[++j] ?? ""; continue; }
        if (src[j] === "$" && src[j + 1] === "(") { j = paren(j + 2) - 1; text += "$()"; dyn = true; continue; }
        if (src[j] === "`" && !ps && !cmd) { j = tick(j); text += "``"; dyn = true; continue; }
        text += src[j];
      }
      raw += src.slice(i, j + 1); cooked += text; open = true; i = j; continue;
    }
    if (c === "$" && src[i + 1] === "(") { i = paren(i + 2) - 1; raw += "$()"; cooked += "$()"; open = dyn = true; continue; }
    if (c === "`" && !ps && !cmd) { i = tick(i); raw += "`"; cooked += "`"; open = dyn = true; continue; }
    if (!ps && !cmd && c === "<" && src[i + 1] === "<" && src[i + 2] !== "<") {   // heredoc: <<[-]['"]WORD['"]
      const m = /^<<(-?)\s*(['"]?)([A-Za-z_][\w-]*)\2/.exec(src.slice(i));
      if (m) { pending.push({ strip: !!m[1], delim: m[3], segIndex: segs.length }); i += m[0].length - 1; continue; }
    }
    if ((c === "$" && !open) || (c === "%" && cmd)) dyn = true;               // $cmd args / %CMD% args
    if (c === "\n" || c === "\r" || c === ";") {
      seg();
      if (c === "\n" && pending.length) {                                      // cut each heredoc body out of the stream
        for (const hd of pending.splice(0)) {
          const lines = []; let k = i + 1;
          while (k < src.length) {
            let e = src.indexOf("\n", k); if (e < 0) e = src.length;
            const line = src.slice(k, e).replace(/\r$/, "");
            k = e + 1;
            if ((hd.strip ? line.replace(/^\t+/, "") : line) === hd.delim) break;
            lines.push(line);
          }
          heredocs.push({ segIndex: hd.segIndex, body: lines.join("\n") }); i = k - 1;
        }
      }
      continue;
    }
    if (c === "|" || (c === "&" && !ps && !/[<>]/.test(src[i - 1] ?? ""))) { seg(); continue; }
    if (ps && (src.startsWith("&&", i) || src.startsWith("||", i))) { seg(); i++; continue; }   // PS7 chains
    if (c === "(" || c === ")") { seg(); continue; }
    if ((c === "{" || c === "}") && !open && (ps || /\s|^$/.test(src[i + 1] ?? " "))) { seg(); continue; }
    if (c === " " || c === "\t") { word(); continue; }
    if (c === "#" && !open && !cmd) { while (i < src.length && src[i] !== "\n") i++; i--; continue; }   // keep the newline
    raw += c; cooked += c; open = true;
  }
  seg();
  return { segs, nested, heredocs };
}

// ---- vocab
const strip = (s) => s.toLowerCase().replace(/\.(exe|com|cmd|bat|ps1)$/, "");
const hit = (id, why) => ({ id, why });

const ALWAYS = new Set(["rm","rmdir","unlink","shred","srm","trash","trash-put","trash-empty","rmtrash","rimraf","del","erase","rd","deltree",
  "sdelete","sdelete64","remove-item","ri","remove-itemproperty","clear-recyclebin","clear-disk","wipefs","diskpart","format","gvfs-trash"]);
const PS_REMOVE_OK = new Set(["remove-variable","remove-alias","remove-module","remove-psdrive","remove-job","remove-pssession",
  "remove-event","remove-typedata","remove-psbreakpoint"]);
const PREFIX = new Set(["command","builtin","exec","nohup","setsid","time","!","if","then","else","elif","do","while","until","&",".","call"]);
const WRAPPER = new Set(["sudo","doas","pkexec","su","runuser","env","nice","ionice","stdbuf","timeout","taskset","chrt","flock","watch",
  "xargs","parallel","ssh","wsl","nsenter","chroot","docker","podman","kubectl","oc","busybox","toybox","script","rtk","for","start","forfiles",
  "start-process","saps","invoke-command","icm","start-job","start-threadjob","gio"]);
const SHELLS = /^(sh|bash|zsh|dash|ksh|fish|ash)$/;
const INTERP = /^(python[\d.]*|py|pypy3?|node|nodejs|bun|deno|ruby|perl|php|lua|osascript)$/;
const READERS = new Set(["cat","rg","grep","egrep","fgrep","ls","dir","type","get-content","gc","select-string","sls","head","tail","less","more",
  "stat","wc","jq","bat","file","echo","printf","write-output","write-host"]);
const DELETE_LEXEME = /(?:^|[^\w-])(?:rm|rmdir|del|erase|unlink|shred|rd|trash|remove-item|delete)(?![\w-])/i;
const DECODER = /base64|xxd\s+-r|openssl\s+enc|certutil|FromBase64String|\[Convert\]/i;
const SPECIAL = /^(cmd|powershell|pwsh|eval|iex|invoke-expression|remove-.+|mkfs(\..*)?)$/;

// The command a word names. The cooked word wins (`r\m` is rm); a Windows
// path under POSIX quoting (`C:\x\rm.exe`, backslashes eaten) falls back to
// the raw spelling.
const base = (s) => strip(s.replace(/["']/g, "").split(/[\\/]/).pop());
const known = (n) => ALWAYS.has(n) || PREFIX.has(n) || WRAPPER.has(n) || Object.hasOwn(RULES, n)
  || SHELLS.test(n) || INTERP.test(n) || SPECIAL.test(n);
const nameOf = (w) => { const c = base(w.cooked); if (known(c)) return c; const r = base(w.raw); return known(r) ? r : c; };

const PS_WHOLE = [
  [/\[\s*(?:System\.)?IO\.(?:File|Directory)\s*\]\s*::\s*Delete/i, "ps-dotnet-delete"],
  [/\.\s*Delete\s*\(/, "ps-method-delete"],
  [/\b(?:DeleteFile|DeleteFolder|DeleteDirectory)\b/i, "ps-vb-delete"],
  [/FileSystem::Delete/i, "ps-fs-delete"],
  [/\b(?:set-alias|new-alias|sal|nal)\b[^\n;|]*\b(?:remove-item|rm|del|erase|rd|ri|rmdir)\b/i, "ps-alias-to-delete"],
];
const CODE = [   // embedded code in interpreter -c/-e bodies and heredocs
  /\bos\.(?:remove|unlink|rmdir|removedirs)\b/, /\bshutil\.rmtree\b/, /\.(?:unlink|unlinkSync|rmdir|rmdirSync|rm|rmSync)\s*\(/, /\bsend2trash\b/,
  /\bfs\.(?:promises\.)?(?:rm|rmSync|unlink|unlinkSync|rmdir|rmdirSync)\b/, /\brimraf\b/, /\bDeno\.remove/, /\bfse\.remove/,
  /\bFile\.delete\b/, /\bFileUtils\.rm/, /\bunlink\b\s*[("'$]/, /\brmtree\b/, /\bos\.Remove(?:All)?\b/, /\bfs::remove_/, /\bFiles\.delete/,
  /(?:os\.system|subprocess\.\w+|child_process\.\w+|\bexec\w*|\bspawn\w*|system)\s*\([^)]*\b(?:rm|del|rmdir|Remove-Item|unlink)\b/,
];
const scanCode = (text) => CODE.some((re) => re.test(text)) ? hit("code-delete", "embedded code deletes files") : undefined;
const codeArgs = (args) => {
  const out = [];
  for (let k = 0; k < args.length; k++)
    if (/^(-[a-zA-Z]*[cepE]|--eval|--print|-r)$/.test(args[k].cooked) && args[k + 1]) out.push(args[++k].cooked);
  return out.join("\n");
};

// ---- the classifier
export function classifyShell(src, dialect = "posix") {
  const budget = { steps: 4096 };
  const scan = (text, d, depth) => {
    const ctx = {
      dialect: d,
      budget,
      opaque: () => DELETE_LEXEME.test(text) ? hit("opaque-dynamic", "a command built at run time next to a delete word") : undefined,
      stdin: () => (DELETE_LEXEME.test(text) || DECODER.test(text)) ? hit("stdin-shell", "a shell or interpreter reads a script from a pipe next to a delete word") : undefined,
      script: (t, d2, dp) => scan(t, d2, dp),
    };
    if (depth > 8 || text.length > 262144) return DELETE_LEXEME.test(text) ? hit("too-deep", "input too large or deeply nested to analyse") : undefined;
    if (d === "ps") for (const [re, id] of PS_WHOLE) if (re.test(text)) return hit(id, "PowerShell deletes via .NET or an alias");
    const { segs, nested, heredocs } = split(text, d);
    for (const words of segs) { const r = inspect(words, ctx, depth); if (r) return { ...r, seg: r.seg ?? words.map((w) => w.raw).join(" ") }; }
    for (const hd of heredocs) {          // scan only if the segment that opened it runs an interpreter
      const head = segs[hd.segIndex]?.find((w) => !/^[A-Za-z_]\w*=/.test(w.raw)); const n = head && nameOf(head);
      const r = n && (SHELLS.test(n) ? scan(hd.body, "posix", depth + 1) : INTERP.test(n) ? scanCode(hd.body) : undefined); if (r) return r;
    }
    for (const n of nested) { const r = scan(n.src, n.dialect, depth + 1); if (r) return r; }
  };
  return scan(String(src), dialect, 0);
}

const first = (args, skip = /^-/) => args.find((a) => !skip.test(a.cooked));
const text = (ws) => ws.map((w) => w.cooked);

function inspect(words, ctx, depth) {            // ONE simple command
  if (depth > 8 || --ctx.budget.steps < 0) return ctx.opaque();
  let i = 0;
  while (i < words.length && /^[A-Za-z_]\w*=/.test(words[i].raw)) i++;          // FOO=1 cmd
  const head = words[i]; if (!head) return;
  if (head.dyn) return ctx.opaque();                                            // $cmd, %CMD%, $(...) as command
  const name = nameOf(head), args = words.slice(i + 1);

  if (PREFIX.has(name)) {
    if (name === "command" && /^-[vV]$/.test(args[0]?.cooked ?? "")) return;    // command -v rm: a lookup
    return inspect(args, ctx, depth + 1);
  }
  if (WRAPPER.has(name)) {
    for (let k = 0; k < args.length; k++) {                                     // `sudo -u root rm x`: rm is at k=2
      const r = inspect(args.slice(k), ctx, depth + 1); if (r) return r;
      if (/\s/.test(args[k].cooked)) {                                          // ssh h "rm x"
        for (const d of new Set(["posix", ctx.dialect])) { const s = ctx.script(args[k].cooked, d, depth + 1); if (s) return s; }
      }
    }
  }
  if (ALWAYS.has(name) || /^mkfs(\.|$)/.test(name)) return hit(`cmd-${name}`, `${name} deletes or destroys data`);
  if (ctx.dialect === "ps" && /^remove-/.test(name) && !PS_REMOVE_OK.has(name)) return hit("ps-remove", `${name} removes data`);
  if (SHELLS.test(name)) {
    const k = args.findIndex((a) => /^-[a-z]*c[a-z]*$/.test(a.cooked));
    if (k >= 0 && args[k + 1]) return ctx.script(args[k + 1].cooked, "posix", depth + 1);
    if (!first(args)) return ctx.stdin();                                       // `echo 'rm x' | sh`
  }
  if (name === "cmd") {
    for (let k = 0; k < args.length; k++) {
      const c = args[k].cooked;
      if (/^\/{1,2}[ckr]$/i.test(c)) return ctx.script(text(args.slice(k + 1)).join(" "), "cmd", depth + 1);
      const m = /^\/{1,2}[ckr]\s+([\s\S]*)$/i.exec(c);                          // -ArgumentList "/c del x"
      if (m) return ctx.script([m[1], ...text(args.slice(k + 1))].join(" "), "cmd", depth + 1);
    }
  }
  if (name === "powershell" || name === "pwsh") return psArgs(args, ctx, depth);
  if (name === "eval" || name === "iex" || name === "invoke-expression")
    return ctx.script(text(args).join(" "), name === "eval" ? "posix" : "ps", depth + 1);
  if (INTERP.test(name)) {
    const m = args.findIndex((a) => a.cooked === "-m");                        // python -m pip uninstall x
    if (m >= 0 && args[m + 1]) { const r = inspect(args.slice(m + 1), ctx, depth + 1); if (r) return r; }
    const body = codeArgs(args); if (body) { const r = scanCode(body); if (r) return r; }
    else if (!first(args) || args.some((a) => a.cooked === "-")) return ctx.stdin();
  }
  if (hasHttpDelete(name, args)) return hit("http-delete", "an HTTP DELETE request");
  return RULES[name]?.(args, ctx, depth);
}

function psArgs(args, ctx, depth) {
  const VALUED = /^-(executionpolicy|ep|workingdirectory|wd|version|inputformat|outputformat|windowstyle|w|configurationname|settingsfile|file|f)$/i;
  for (let k = 0; k < args.length; k++) {
    const c = args[k].cooked;
    if (/^-(c|command|commandwithargs)$/i.test(c)) return ctx.script(text(args.slice(k + 1)).join(" "), "ps", depth + 1);
    if (/^-(e|ec|enc|encodedcommand)$/i.test(c) && args[k + 1]) {
      let decoded; try { decoded = Buffer.from(args[k + 1].cooked, "base64").toString("utf16le"); } catch { /* below */ }
      if (!decoded || decoded.includes("\u0000")) return hit("ps-encoded", "an encoded PowerShell command that cannot be read");
      return ctx.script(decoded, "ps", depth + 1);
    }
    if (VALUED.test(c)) { k++; continue; }
    if (!c.startsWith("-")) return ctx.script(text(args.slice(k)).join(" "), "ps", depth + 1);
  }
}

const hasHttpDelete = (name, a) => {
  const c = text(a);
  for (let k = 0; k < c.length; k++) {
    if (/^(-X|--request|-method|--method)$/i.test(c[k]) && /^delete$/i.test(c[k + 1] ?? "")) return true;
    if (/^(--request=|--method=|-X)delete$/i.test(c[k])) return true;
  }
  return /^(http|https|xh|curlie)$/.test(name) && /^delete$/i.test(c[0] ?? "");
};

const sqlBad = /\b(?:drop\s+(?:table|database|schema|index|view)|delete\s+from|truncate)\b/i;
const PM_BAD = /^(uninstall|remove|rm|un|unlink|purge|autoremove|autoclean|prune|cleanup|clean|delete)$/;
const pm = (a) => { const s = first(a)?.cooked.toLowerCase(); return s && PM_BAD.test(s) ? hit("pkg-remove", `${s} removes installed packages`) : undefined; };
const sql = (a) => sqlBad.test(text(a).join(" ")) ? hit("sql-delete", "an SQL statement that drops or deletes data") : undefined;
const cloud = (a) => text(a).some((x) => /^(delete|rm|rb|remove|destroy|purge|terminate)(-|$)/i.test(x)) ? hit("cloud-delete", "a cloud CLI delete") : undefined;
const nestedAgent = (why) => hit("agent-nested", why ?? "a nested agent CLI runs unguarded");
const RULES = {
  find: (a, ctx, d) => { for (let k = 0; k < a.length; k++) { const c = a[k].cooked;
      if (c === "-delete") return hit("find-delete", "find -delete removes files");
      if (/^-(exec|execdir|ok|okdir)$/.test(c)) { let e = k + 1; while (e < a.length && a[e].cooked !== ";" && a[e].cooked !== "+") e++;
        const r = inspect(a.slice(k + 1, e), ctx, d + 1); if (r) return r; } } },
  fd: (a, ctx, d) => { const k = a.findIndex((x) => /^(-x|-X|--exec|--exec-batch)$/.test(x.cooked)); return k >= 0 ? inspect(a.slice(k + 1), ctx, d + 1) : undefined; },
  rsync: (a) => text(a).some((x) => /^--(delete|del|remove-source-files)/.test(x)) ? hit("rsync-delete", "rsync deletes files") : undefined,
  tar: (a) => text(a).includes("--remove-files") ? hit("tar-remove", "tar --remove-files") : undefined,
  zip: (a) => text(a).some((x) => x === "-m" || x === "--move") ? hit("zip-move", "zip -m deletes the sources") : undefined,
  "7z": (a) => text(a).some((x) => /^-sdel$/i.test(x)) ? hit("7z-sdel", "7z -sdel") : undefined,
  mv: (a) => /(^\/dev\/null$|^nul$|^\$null$|\.Trash|\$Recycle\.Bin)/i.test(text(a).at(-1) ?? "") ? hit("mv-trash", "mv into the void or the bin") : undefined,
  robocopy: (a) => text(a).some((x) => /^\/(mir|purge|mov|move)$/i.test(x)) ? hit("robocopy-purge", "robocopy mirrors/moves and deletes") : undefined,
  cipher: (a) => text(a).some((x) => /^\/w/i.test(x)) ? hit("cipher-wipe", "cipher /w wipes free space") : undefined,
  reg: (a) => /^delete$/i.test(first(a, /^\//)?.cooked ?? "") ? hit("reg-delete", "reg delete") : undefined,
  sc: (a) => /^delete$/i.test(first(a)?.cooked ?? "") ? hit("sc-delete", "sc delete") : undefined,
  schtasks: (a) => text(a).some((x) => /^\/delete$/i.test(x)) ? hit("schtasks-delete", "schtasks /delete") : undefined,
  dd: (a) => text(a).some((x) => /^of=(?!\/dev\/(null|stdout)$)/.test(x)) ? hit("dd-of", "dd writes over its target") : undefined,
  osascript: (a) => /\b(delete|trash)\b/i.test(text(a).join(" ")) ? hit("osascript-delete", "AppleScript delete") : undefined,
  git: gitRule,
  docker: dockerRule, podman: dockerRule,
  kubectl: (a) => text(a).includes("delete") ? hit("kubectl-delete", "kubectl delete") : undefined, oc: (a) => RULES.kubectl(a),
  helm: (a) => /^(uninstall|delete)$/.test(first(a)?.cooked ?? "") ? hit("helm-uninstall", "helm uninstall") : undefined,
  terraform: (a) => { const t = text(a); return t[0] === "destroy" || (t[0] === "state" && t[1] === "rm") ? hit("tf-destroy", "terraform destroy") : undefined; },
  tofu: (a) => RULES.terraform(a),
  gh: (a) => text(a).includes("delete") ? hit("gh-delete", "gh delete") : undefined,
  aws: cloud, gcloud: cloud, az: cloud, gsutil: cloud, doctl: cloud,
  psql: sql, mysql: sql, mariadb: sql, sqlite3: sql, sqlcmd: sql, mongosh: sql,
  "redis-cli": (a) => text(a).some((x) => /^(flushall|flushdb|del|unlink)$/i.test(x)) ? hit("redis-delete", "redis delete") : undefined,
  npm: pm, pnpm: pm, yarn: pm, bun: pm, pip: pm, pip3: pm, pipx: pm, uv: pm, cargo: pm, brew: pm, apt: pm, "apt-get": pm,
  dnf: pm, yum: pm, choco: pm, winget: pm, scoop: pm, gem: pm, go: pm,
  pi: (a) => text(a).some((x) => x === "--no-extensions" || x === "-ne") ? hit("agent-no-ext", "a nested Pi started without extensions would run unguarded") : undefined,
  claude: (a) => text(a).some((x) => x === "-p" || x === "--print") ? nestedAgent() : undefined,
  codex: (a) => /^(exec|e|review)$/.test(first(a)?.cooked ?? "") ? nestedAgent() : undefined,
  opencode: (a) => first(a)?.cooked === "run" ? nestedAgent() : undefined,
  gemini: (a) => text(a).some((x) => /^(-p|--prompt)$/.test(x)) ? nestedAgent() : undefined,
  "cursor-agent": (a) => text(a).some((x) => x === "-p") ? nestedAgent() : undefined,
  aider: (a) => text(a).some((x) => /^(-m|--message)$/.test(x)) ? nestedAgent() : undefined,
};
function dockerRule(a) {
  const t = text(a).map((x) => x.toLowerCase());
  if (/^(rm|rmi)$/.test(t[0] ?? "")) return hit("docker-rm", "docker rm/rmi");
  if (t[0] === "compose" && /^(rm|down)$/.test(t[1] ?? "")) return hit("docker-compose-down", "docker compose rm/down");
  if (/^(container|image|volume|network|system|builder|buildx)$/.test(t[0] ?? "") && t.slice(1).some((x) => /^(rm|remove|prune)$/.test(x))) return hit("docker-prune", "docker rm/prune");
}
function gitRule(a) {
  let k = 0;
  while (k < a.length) { const c = a[k].cooked;
    if (/^(-C|-c|--git-dir|--work-tree|--namespace|--exec-path|--super-prefix)$/.test(c)) k += 2; else if (c.startsWith("-")) k++; else break; }
  const sub = a[k]?.cooked.toLowerCase(), rest = text(a.slice(k + 1));
  const dry = rest.some((x) => /^(--dry-run|-[A-Za-z]*n[A-Za-z]*)$/.test(x));
  switch (sub) {
    case "clean": return dry ? undefined : hit("git-clean", "git clean deletes untracked files");
    case "rm": return rest.some((x) => ["--cached", "-n", "--dry-run", "--help"].includes(x)) ? undefined : hit("git-rm", "git rm deletes tracked files");
    case "branch": return rest.some((x) => /^(--delete|-[A-Za-z]*[dD][A-Za-z]*)$/.test(x)) ? hit("git-branch-delete", "git branch -d/-D") : undefined;
    case "tag": return rest.some((x) => /^(-d|--delete)$/.test(x)) ? hit("git-tag-delete", "git tag -d") : undefined;
    case "push": return rest.some((x) => /^(--delete|-d|--mirror|--prune)$/.test(x) || /^\+?:[^:]/.test(x)) ? hit("git-push-delete", "git push deletes remote refs") : undefined;
    case "stash": return /^(drop|clear)$/.test(rest[0] ?? "") ? hit("git-stash-drop", "git stash drop/clear") : undefined;
    case "worktree": return /^(remove|prune)$/.test(rest[0] ?? "") ? hit("git-worktree-remove", "git worktree remove/prune") : undefined;
    case "remote": return /^(remove|rm|prune)$/.test(rest[0] ?? "") ? hit("git-remote-remove", "git remote remove/prune") : undefined;
    case "reflog": return /^(expire|delete)$/.test(rest[0] ?? "") ? hit("git-reflog", "git reflog expire/delete") : undefined;
    case "gc": return rest.some((x) => x.startsWith("--prune")) ? hit("git-gc-prune", "git gc --prune") : undefined;
    case "update-ref": return rest.includes("-d") ? hit("git-update-ref-d", "git update-ref -d") : undefined;
    case "submodule": return rest[0] === "deinit" ? hit("git-submodule-deinit", "git submodule deinit") : undefined;
    case "filter-branch": case "filter-repo": case "prune": case "prune-packed": return hit(`git-${sub}`, `git ${sub} rewrites or deletes objects`);
  }
}

// ---- tool calls
const READ = new Set(["get","list","read","search","find","count","check","is","has","can","show","describe","preview","dry","test","validate","status","view","fetch","query","inspect"]);
const WRITE = new Set(["create","update","set","save","add","start","stop","restart","reload","deploy","move","import","enable","disable","toggle","run","send","assign","generate","register","duplicate","rebuild","redeploy","rollback","kill","cancel","change","upload","write","reset"]);
const DEL = new Set(["delete","deletion","remove","removal","destroy","drop","purge","prune","trash","erase","wipe","unlink","uninstall","discard","forget","clean","cleanup","clear","truncate","rm","rmdir"]);
export function nameTokens(name) {                       // mcp__dokploy_kvm4__application_delete -> [application, delete]
  const tool = name.startsWith("mcp__") ? name.split("__").slice(2).join("_") : name;
  return tool.replace(/([a-z0-9])([A-Z])/g, "$1 $2").split(/[^A-Za-z0-9]+/).filter(Boolean).map((t) => t.toLowerCase());
}
export function classifyCall({ toolName, input = {}, annotations, cwd = process.cwd(), fs = { statSync } }) {
  if (toolName === "bash" || toolName === "powershell") return classifyShell(String(input.command ?? ""), toolName === "bash" ? "posix" : "ps");
  if (toolName === "write" && typeof input.path === "string" && (input.content ?? "") === "") {
    try { if (fs.statSync(resolve(cwd, input.path)).size > 0) return hit("write-truncate", "an empty write would erase an existing file"); } catch { /* new file */ }
  }
  if (/(bash|shell|exec|terminal|run_command|powershell|pwsh)/i.test(toolName))
    for (const k of ["command", "cmd", "script"]) if (typeof input[k] === "string") { const r = classifyShell(input[k], "posix"); if (r) return r; }
  if (String(input.method ?? "").toUpperCase() === "DELETE") return hit("http-delete", "an HTTP DELETE");
  if (annotations?.destructiveHint === true) return hit("tool-destructive", `${toolName} is marked destructive`);
  const verb = nameTokens(toolName).find((t) => READ.has(t) || WRITE.has(t) || DEL.has(t));  // first verb decides
  if (verb && DEL.has(verb)) return hit("tool-name", `${toolName} deletes data`);
}

// ---- self-protection: a model must not switch the guard off through files, env or the IPC port
const SELF = /harness-prefs|zeron-delete-guard|ZERON_DELETE_GUARD|ZERON_BLOCK_DELETES|ZERON_IPC_PORT|SetDeleteGuard|SetPiSettings|SetHarnessEnabled/i;
const PROTECTED_FILE = /^(harness-prefs\.json(\.tmp)?|zeron-delete-guard-[0-9a-f]+\.mjs|\.zeron-delete-guard-.*\.tmp)$/i;
// web_search and web_fetch run on DeepSeek's servers and change nothing here.
const READONLY_TOOLS = new Set(["read", "grep", "find", "ls", "web_search", "web_fetch"]);
export function selfProtect({ toolName, input = {} }) {
  const deny = () => hit("guard-self", "this would change or bypass the delete guard");
  if (READONLY_TOOLS.has(toolName)) return;
  if (toolName === "bash" || toolName === "powershell") {
    const src = String(input.command ?? ""); if (!SELF.test(src)) return;
    const { segs } = split(src, toolName === "bash" ? "posix" : "ps");
    let found = false, readerOnly = true;
    for (const s of segs) { const t = s.map((w) => w.raw).join(" "); if (!SELF.test(t)) continue; found = true;
      if (!READERS.has(nameOf(s[0])) || s.some((w) => /^[^'"]*>/.test(w.raw))) readerOnly = false; }
    return !found || !readerOnly ? deny() : undefined;     // not in any segment = inside -c/heredoc/nested: block
  }
  for (const k of ["path", "file", "filePath", "file_path", "target", "destination", "dest"])
    if (typeof input[k] === "string" && PROTECTED_FILE.test(basename(input[k]))) return deny();
  if (SELF.test(JSON.stringify(input).slice(0, 65536))
      && /(shell|exec|command|fetch|http|request|socket|script)/i.test(toolName)) return deny();
}

const REASON = (h) =>
  `[zeron:delete-guard rule=${h.id}] Blocked: ${h.why}` +
  `${h.seg ? ` (matched: ${h.seg.slice(0, 120)})` : ""}. Delete protection is on for every agent in this workspace. ` +
  `Do not retry, rephrase, or get the same effect through another tool, script, interpreter or subagent. ` +
  `If something must be removed, stop and tell the user exactly what and why. They can do it themselves or switch off ` +
  `Block deletes in Zeron Settings > General > Pi. Continue the rest of the task without the deletion. ` +
  `If you are a subagent, report this blocked deletion in your final answer.`;

export function decide(event, annotations, cwd) {
  return selfProtect(event) ?? classifyCall({ toolName: event.toolName, input: event.input ?? {}, annotations, cwd });
}

// ---- children: every Pi this one starts loads the guard too. Pi's child
// launchers re-run this script with its loader flags (`process.execArgv`), so
// the preload joins them; it hands each child Pi `--extension <guard>`. This
// Pi's script is named so the preload touches no other node program.
export function inheritIntoChildren(proc = process) {
  const preload = proc.env.ZERON_DELETE_GUARD_PRELOAD;
  if (!preload || !proc.env.ZERON_DELETE_GUARD_EXT) return false;
  if (proc.argv[1]) proc.env.ZERON_DELETE_GUARD_PI = resolve(proc.argv[1]);
  const url = pathToFileURL(preload).href;
  const flags = proc.execArgv;
  if (!flags.some((arg, i) => arg === `--import=${url}` || (arg === "--import" && flags[i + 1] === url)))
    flags.push(`--import=${url}`);
  return true;
}

const SHELL_TOOLS = new Set(["bash", "powershell"]);
// ---- children of a compiled Pi: a single executable takes no node flags,
// so the preload cannot ride along. Its launchers start the same executable
// again (`piInvocation`), so the guard wraps child_process here: each spawn
// of this executable gets `--extension <guard>`, and inherits the env.
const NODE_RUNTIME = /^(node|bun)(\.exe)?$/i;
const samePath = (a, b, platform = process.platform) =>
  platform === "win32" ? resolve(a).toLowerCase() === resolve(b).toLowerCase() : resolve(a) === resolve(b);

export function guardSpawnArgs(command, args, ext, execPath, platform = process.platform) {
  if (!ext || typeof command !== "string" || !Array.isArray(args)) return args;
  if (!samePath(command, execPath, platform)) return args;
  if (args.some((arg, i) => arg === ext && (args[i - 1] === "-e" || args[i - 1] === "--extension"))) return args;
  return ["--extension", ext, ...args];
}

const SPAWNERS = ["spawn", "spawnSync", "execFile", "execFileSync"];
export function guardCompiledChildren(proc = process, cp = createRequire(import.meta.url)("node:child_process")) {
  const ext = proc.env.ZERON_DELETE_GUARD_EXT;
  if (!ext || NODE_RUNTIME.test(basename(proc.execPath))) return false;
  if (cp.__zeronDeleteGuard) return true;
  for (const name of SPAWNERS) {
    const original = cp[name];
    if (typeof original !== "function") continue;
    cp[name] = function (command, ...rest) {
      if (Array.isArray(rest[0])) rest[0] = guardSpawnArgs(command, rest[0], ext, proc.execPath, proc.platform);
      return original.call(this, command, ...rest);
    };
  }
  Object.defineProperty(cp, "__zeronDeleteGuard", { value: true });
  // `import { spawn } from "node:child_process"` sees the wrapper too.
  try { syncBuiltinESMExports(); } catch {}
  return true;
}

// ---- read-only sessions. Only reading tools, read-only shell commands and
// tools whose names do not say they change something pass. Children inherit
// the flag through env, so a subagent of a read-only session is read-only.
export const readOnlyOn = (env = process.env) => env.ZERON_READ_ONLY === "1";
const RO_COMMANDS = new Set([...READERS, "find", "fd", "tree", "pwd", "which", "whereis", "where", "printenv", "date",
  "uname", "du", "df", "sort", "uniq", "cut", "tr", "diff", "cmp", "basename", "dirname", "realpath", "readlink", "nl",
  "column", "od", "hexdump", "sha256sum", "sha1sum", "md5sum", "b3sum", "true", "false", "test", "[", "cd", "ps", "whoami",
  "id", "hostname", "ag", "ack", "tokei", "cloc", "get-childitem", "gci", "get-item", "test-path", "resolve-path",
  "get-location", "measure-object", "sort-object", "select-object", "where-object", "format-table", "format-list",
  "out-string", "get-command", "get-process", "get-date"]);
const RO_WRAPPERS = new Set(["xargs", "timeout", "nice", "env", "command", "time", "rtk", "nohup", "stdbuf", "ionice"]);
const GIT_READ = new Set(["status", "log", "diff", "show", "blame", "annotate", "grep", "ls-files", "ls-tree", "rev-parse",
  "rev-list", "describe", "shortlog", "cat-file", "whatchanged", "merge-base", "name-rev", "for-each-ref", "show-ref",
  "show-branch", "count-objects", "cherry", "range-diff", "help", "version"]);
const GIT_GLOBAL_VALUED = new Set(["-C", "-c", "--git-dir", "--work-tree", "--namespace"]);
const GIT_LISTING = /^--?(a|r|v|vv|l|list|all|remotes|contains|no-contains|merged|no-merged|sort|format|points-at|show-current|column|no-column|color|no-color|verbose)(=.*)?$/;
const FIND_WRITES = new Set(["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls"]);
const RO_TOOL_WRITES = new Set([...WRITE, ...DEL, "edit", "patch", "apply", "replace", "insert", "append", "rename", "commit",
  "push", "merge", "install", "mkdir", "touch", "copy", "archive", "respond", "interrupt", "steer", "queue", "publish", "post",
  "put", "schedule", "approve", "register"]);
const NULL_SINK = /^(\/dev\/null|&[0-9-]|\$null|nul)$/i;

export function gitReadOnly(args) {
  let k = 0;
  while (k < args.length && args[k].startsWith("-")) k += GIT_GLOBAL_VALUED.has(args[k]) ? 2 : 1;
  const sub = args[k], rest = args.slice(k + 1);
  if (sub === undefined) return true;
  if (rest.some((a) => /^--output(=|$)/.test(a))) return false;
  if (GIT_READ.has(sub)) return true;
  if (sub === "branch" || sub === "tag") return rest.every((a) => GIT_LISTING.test(a));
  if (sub === "remote") return rest.every((a) => a === "-v" || a === "--verbose") || rest[0] === "show" || rest[0] === "get-url";
  if (sub === "stash") return rest[0] === "list" || rest[0] === "show";
  if (sub === "worktree") return rest[0] === "list";
  if (sub === "reflog") return rest.length === 0 || rest[0] === "show";
  if (sub === "config")
    return rest.some((a) => /^(--get|--get-all|--get-regexp|--list|-l)$/.test(a))
      && !rest.some((a) => /^--(add|unset|unset-all|replace-all|rename-section|remove-section|edit)$/.test(a) || a === "-e");
  return false;
}

function roRedirect(words) {                    // `> file` writes; `2>&1`, `>/dev/null` do not
  for (let k = 0; k < words.length; k++) {
    const raw = words[k].raw; if (!/^[^'"]*>/.test(raw)) continue;
    const target = raw.slice(raw.lastIndexOf(">") + 1) || words[k + 1]?.cooked || "";
    if (!NULL_SINK.test(target)) return hit("ro-redirect", "output redirected into a file");
  }
}

function roSegment(words, dialect, depth) {
  if (depth > 8) return hit("ro-too-deep", "a command too deeply nested to check");
  let i = 0;
  while (i < words.length && /^[A-Za-z_]\w*=/.test(words[i].raw)) i++;
  const head = words[i]; if (!head) return;
  const redirect = roRedirect(words); if (redirect) return redirect;
  if (head.dyn) return hit("ro-dynamic", "a command built at run time");
  const name = nameOf(head), args = words.slice(i + 1), cooked = args.map((a) => a.cooked);
  if (RO_WRAPPERS.has(name)) {
    let k = 0;
    while (k < args.length && (/^-/.test(args[k].cooked) || /^[A-Za-z_]\w*=/.test(args[k].raw) || /^\d+[smhd]?$/.test(args[k].cooked))) k++;
    return args[k] ? roSegment(args.slice(k), dialect, depth + 1) : undefined;
  }
  if (name === "git") return gitReadOnly(cooked) ? undefined : hit("ro-git", "a git command that changes the repository");
  if (name === "find" && cooked.some((a) => FIND_WRITES.has(a))) return hit("ro-find", "find that deletes, runs commands or writes");
  if (name === "sort" && cooked.some((a) => /^(-o|--output)/.test(a))) return hit("ro-write", "sort writing a file");
  if (RO_COMMANDS.has(name)) return;
  return hit("ro-command", `${name} may change files`);
}

export function shellWrites(src, dialect = "posix", depth = 0) {
  if (depth > 8 || src.length > 262144) return hit("ro-too-deep", "input too large or deeply nested to check");
  const { segs, nested } = split(src, dialect);
  for (const words of segs) {
    const r = roSegment(words, dialect, depth);
    if (r) return { ...r, seg: r.seg ?? words.map((w) => w.raw).join(" ") };
  }
  for (const n of nested) { const r = shellWrites(n.src, n.dialect, depth + 1); if (r) return r; }
}

// Zeron's own tools that change no file: a read-only session still posts
// what it finds to its orchestrator's board, and answers the sessions that
// message it. Starting sessions stays blocked (a writer would escape).
const RO_ZERON_TOOLS = new Set(["zeron_board_post", "zeron_board_read", "zeron_send_message", "zeron_send_messages"]);

// A read-only session researches through sub-agents, so subagent_start passes
// when every task only reads (writeAccess absent or false; the sub-agents
// inherit the flag through env and are held to it as well). Anything else
// fails closed, a string "true" included, and so does a call with no task list
// to read. Workflow agents run in-process with tools outside this guard, so
// starting one is refused.
export const launchWrites = (input) =>
  !Array.isArray(input?.tasks) ||
  input.tasks.some((t) => !t || typeof t !== "object" || (t.writeAccess !== undefined && t.writeAccess !== false));
const RO_WORKFLOW_TOOLS = new Set(["workflow", "workflow_script"]);

export function classifyReadOnly({ toolName, input = {}, annotations }) {
  if (READONLY_TOOLS.has(toolName) || RO_ZERON_TOOLS.has(toolName)) return;
  if (toolName === "write" || toolName === "edit") return hit("ro-edit", `${toolName} changes files`);
  if (toolName === "subagent_start")
    return launchWrites(input) ? hit("ro-subagent-writer", "subagent_start must list tasks that all leave writeAccess false: a read-only session starts sub-agents that only read") : undefined;
  if (RO_WORKFLOW_TOOLS.has(toolName))
    return hit("ro-workflow", "workflow agents can write: a read-only session researches with subagent_start (writeAccess false)");
  if (SHELL_TOOLS.has(toolName)) return shellWrites(String(input.command ?? ""), toolName === "bash" ? "posix" : "ps");
  if (/(bash|shell|exec|terminal|run_command|powershell|pwsh)/i.test(toolName))
    return hit("ro-shell", `${toolName} runs commands this guard cannot check`);
  if (annotations?.readOnlyHint === true) return;
  if (annotations?.destructiveHint === true) return hit("ro-destructive", `${toolName} is marked destructive`);
  const verb = nameTokens(toolName).find((t) => RO_TOOL_WRITES.has(t));
  if (verb) return hit("ro-tool", `${toolName} changes something (${verb})`);
}

const READ_ONLY_REASON = (h) =>
  `[zeron:read-only rule=${h.id}] Blocked: ${h.why}${h.seg ? ` (matched: ${h.seg.slice(0, 120)})` : ""}. ` +
  `This session is read-only: it was started to read, search and report, not to change anything. ` +
  `Do not retry, rephrase, or get the same effect through another tool, script, interpreter or a subagent that writes. ` +
  `Finish with what reading tells you, and say in your final answer what you would change and where.`;

// ---- the Pis below the session: each guarded Pi records its pid and its
// level below the Zeron session (`ZERON_PI_PID`, `ZERON_SUBAGENT_DEPTH`) for
// the Pis it starts; Zeron clears both on every run it starts. Keyed by pid,
// so Pi re-running this factory in the same process (a reload, a new
// session) keeps its level. Only the session itself (level 0) registers
// /reload and runs the package check. What a sub-agent is told about its
// place is the Pi harness's to say (Zeron publishes the facts as env).
export function piDepth(env, pid) {
  const level = Number(env.ZERON_SUBAGENT_DEPTH) || 0;
  if (!env.ZERON_PI_PID) return 0;
  return env.ZERON_PI_PID === String(pid) ? level : level + 1;
}

// Zeron asks (`ZERON_PACKAGE_CHECK=1`, at most twice a day) which installed
// Pi packages have updates, with Pi's own check; Zeron turns the answer into
// a toast. Pi's library is the `index.js` beside the `cli.js` running now.
export const PACKAGE_UPDATES_PREFIX = "ZERON-PACKAGE-UPDATES ";
export async function packageUpdates(cwd, cli = process.argv[1]) {
  const index = join(dirname(realpathSync(cli ?? "")), "index.js");
  if (!existsSync(index)) return undefined;
  const pi = await import(pathToFileURL(index).href);
  if (typeof pi.DefaultPackageManager !== "function") return undefined;
  const agentDir = pi.getAgentDir();
  const settingsManager = pi.SettingsManager.create(cwd, agentDir);
  const manager = new pi.DefaultPackageManager({ cwd, agentDir, settingsManager });
  const updates = await manager.checkForAvailableUpdates();
  return updates.map((u) => ({ name: u.displayName, source: u.source, scope: u.scope }));
}

export const RELOAD_DESCRIPTION = "Reload keybindings, extensions, skills, prompts, themes, and context files";

// ---- session model restrictions. The models a session may not use: Zeron's
// per-chat files (the chat's own, then each parent's, orchestrator included)
// named in `ZERON_MODEL_POLICY`. Every Pi of the session inherits the env and
// the guard (sub-agents, workflows, orchestrator members), so none of them can
// run one. Re-read on change: a list edited mid-session holds from the next
// turn. Global helpers (the advisor, decisions, memory, extension utilities)
// are not the session's and keep their own models.
const POLICY_EXEMPT = new Set(["advisor", "decisions", "jev", "memory"]);
export function policyExempt(env = process.env) {
  const surface = env.PI_USAGE_SURFACE ?? "";
  return POLICY_EXEMPT.has(surface) || surface.startsWith("ext:");
}
const modelKey = (id) => String(id).trim().toLowerCase();
const THINKING_SUFFIX = /:(off|minimal|low|medium|high|xhigh|max)$/;
let policySeen = new Map(); // file -> { mtimeMs, size, models }
export function disabledModels(env = process.env, fs = { statSync, readFileSync }, sep = delimiter) {
  const out = new Set();
  if (!env.ZERON_MODEL_POLICY || policyExempt(env)) return out;
  const next = new Map();
  for (const file of env.ZERON_MODEL_POLICY.split(sep)) {
    if (!file) continue;
    let seen = policySeen.get(file);
    try {
      const st = fs.statSync(file);
      if (!seen || seen.mtimeMs !== st.mtimeMs || seen.size !== st.size) {
        const listed = JSON.parse(fs.readFileSync(file, "utf8"))?.disabled;
        seen = { mtimeMs: st.mtimeMs, size: st.size,
          models: Array.isArray(listed) ? listed.filter((m) => typeof m === "string" && m.trim()).map(modelKey) : [] };
      }
    } catch (e) {
      // Gone: nothing disabled there. Transient I/O (a Windows rename-over):
      // keep the last list.
      if (e?.code === "ENOENT") seen = undefined;
    }
    if (!seen) continue;
    next.set(file, seen);
    for (const model of seen.models) out.add(model);
  }
  policySeen = next;
  return out;
}

/** A model id as Zeron or a tool names it ("provider/id", maybe ":thinking"). */
export function idDisabled(off, id) {
  if (!off.size || typeof id !== "string" || !id.trim()) return false;
  const key = modelKey(id);
  if (off.has(key) || off.has(key.replace(THINKING_SUFFIX, ""))) return true;
  // A bare id names the model on whichever provider serves it.
  if (key.includes("/")) return false;
  const bare = key.replace(THINKING_SUFFIX, "");
  for (const entry of off) if (entry.slice(entry.indexOf("/") + 1) === bare) return true;
  return false;
}
/** `(provider, id)` of a model reference, lower case, without a thinking suffix. */
const splitModelRef = (ref) => {
  const bare = modelKey(ref).replace(THINKING_SUFFIX, "");
  const cut = bare.indexOf("/");
  return cut < 0 ? [undefined, bare] : [bare.slice(0, cut), bare.slice(cut + 1)];
};
/** Whether `entry` names `model` (a bare id or "provider/id", either side): a
 * bare id names the model on any provider. */
export function modelMatches(entry, model) {
  const [entryProvider, entryId] = splitModelRef(entry), [provider, id] = splitModelRef(model);
  return entryId === id && (!entryProvider || !provider || entryProvider === provider);
}
/** A model outside the tree's allow-list (an empty list allows any). */
export const modelNotAllowed = (allowed, ref) =>
  allowed.length > 0 && typeof ref === "string" && ref.trim() !== "" && !allowed.some((entry) => modelMatches(entry, ref));
export const modelDisabled = (off, model, allowed = []) =>
  Boolean(model) && (off.has(modelKey(`${model.provider}/${model.id}`))
    || modelNotAllowed(allowed, `${model.provider}/${model.id}`));

/** The first disabled model a tool call's input names (`model`, `models`, …). */
export function disabledModelIn(input, off, depth = 0) {
  if (!off.size || depth > 5 || !input || typeof input !== "object") return undefined;
  for (const [key, value] of Object.entries(input)) {
    if (/model/i.test(key))
      for (const v of Array.isArray(value) ? value : [value]) if (idDisabled(off, v)) return v;
    if (value && typeof value === "object") {
      const hit = disabledModelIn(value, off, depth + 1);
      if (hit) return hit;
    }
  }
  return undefined;
}

/** The first model a tool call's input names that the allow-list does not admit. */
export function disallowedModelIn(input, allowed, depth = 0) {
  if (!allowed.length || depth > 5 || !input || typeof input !== "object") return undefined;
  for (const [key, value] of Object.entries(input)) {
    if (/model/i.test(key))
      for (const v of Array.isArray(value) ? value : [value]) if (modelNotAllowed(allowed, v)) return v;
    if (value && typeof value === "object") {
      const found = disallowedModelIn(value, allowed, depth + 1);
      if (found) return found;
    }
  }
  return undefined;
}

// The advisor's own `model` is the advisor's (global) choice.
const POLICY_TOOLS_EXEMPT = new Set(["advisor"]);
export const MODEL_REASON = (id) =>
  `Model ${id} is disabled in this session (Zeron). Use a different model.`;

/** A session's allowed model to fall back to: same provider first. */
export function allowedFallback(ctx, off, current, allowed = []) {
  let available = [];
  try { available = ctx.modelRegistry?.getAvailable?.() ?? []; } catch {}
  const pool = [...(ctx.scopedModels ?? []).map((s) => s?.model), ...available]
    .filter((m) => m && !modelDisabled(off, m, allowed));
  return pool.find((m) => m.provider === current?.provider) ?? pool[0];
}

function enforceModels(pi) {
  // Off the disabled model before the turn runs; the request check is the
  // backstop for anything that switched past this.
  const steer = async (ctx, model) => {
    const off = disabledModels(), allowed = allowedModels();
    if (!modelDisabled(off, model, allowed)) return;
    const name = `${model.provider}/${model.id}`;
    const next = allowedFallback(ctx, off, model, allowed);
    let switched = false;
    if (next) { try { switched = await pi.setModel(next); } catch {} }
    try {
      ctx.ui?.notify?.(switched
        ? `${name} is disabled in this session; switched to ${next.provider}/${next.id}.`
        : `${name} is disabled in this session and no allowed model is available.`, switched ? "warning" : "error");
    } catch {}
  };
  pi.on("session_start", (_event, ctx) => steer(ctx, ctx.model));
  pi.on("model_select", (event, ctx) => steer(ctx, event?.model));
  pi.on("before_agent_start", async (_event, ctx) => { await steer(ctx, ctx.model); });
  pi.on("before_provider_request", (_event, ctx) => {
    const halted = treePolicy()?.halted;
    if (halted) {
      // The spending cap halted the tree: no further request is sent.
      try { ctx.ui?.notify?.(`${TREE_MARK} rule=halted] The tree is paused (${halted.reason}); no further model request is made.`, "error"); } catch {}
      ctx.abort();
      return;
    }
    if (!modelDisabled(disabledModels(), ctx.model, allowedModels())) return;
    try { ctx.ui?.notify?.(MODEL_REASON(`${ctx.model.provider}/${ctx.model.id}`), "error"); } catch {}
    ctx.abort();
  });
}

// ---- subscription usage alerts. The engine evaluates the one usage reader
// (the accounts snapshot behind the composer footer) and writes what is low
// to `usage-alerts.json`; this side only reads it. Every guarded Pi (the
// session, its sub-agents, delegates, workflow agents, the advisor) checks
// it: a running Pi on a low account is told to finish its current safe step,
// write a handoff and end; new work aimed at a low account is refused. Never
// a hard stop: no request is aborted, no process killed. Unknown (no file,
// a stale file from a stopped engine, a window past its reset) blocks
// nothing.
export const USAGE_MARK = "[zeron:usage-alert]";
export function usageAlertsPath(env = process.env, home = homedir()) {
  if (env.ZERON_USAGE_ALERTS_FILE) return env.ZERON_USAGE_ALERTS_FILE;
  if (env.ZERON_DATA_DIR) return join(env.ZERON_DATA_DIR, "usage-alerts.json");
  return join(home, ".zeron", "usage-alerts.json");
}
let usageSeen = { file: "", mtimeMs: -1, size: -1, data: undefined };
/** The alerts that hold now: fresh file, window not reset yet. */
export function usageAlerts(env = process.env, fs = { statSync, readFileSync }, now = Date.now()) {
  const file = usageAlertsPath(env);
  let data;
  try {
    const st = fs.statSync(file);
    if (usageSeen.file === file && usageSeen.mtimeMs === st.mtimeMs && usageSeen.size === st.size) data = usageSeen.data;
    else {
      data = JSON.parse(fs.readFileSync(file, "utf8"));
      usageSeen = { file, mtimeMs: st.mtimeMs, size: st.size, data };
    }
  } catch (e) {
    // Gone or corrupt: nothing known. Transient I/O: the last read.
    if (e?.code === "ENOENT" || e instanceof SyntaxError) return [];
    data = usageSeen.file === file ? usageSeen.data : undefined;
  }
  if (!data || data.version !== 1 || !Array.isArray(data.alerts)) return [];
  const updated = Date.parse(data.updatedAt);
  const validMs = (Number(data.validForSecs) || 900) * 1000;
  if (!Number.isFinite(updated) || now - updated > validMs) return [];
  return data.alerts.filter((a) => a && typeof a.key === "string" && a.harness === "pi" && typeof a.provider === "string"
    && (!a.resetsAt || Date.parse(a.resetsAt) > now));
}
export const providerOf = (id) => {
  const text = typeof id === "string" ? id.trim() : "";
  const cut = text.indexOf("/");
  return cut > 0 && cut < text.length - 1 ? text.slice(0, cut).toLowerCase() : undefined;
};
export const alertsOn = (alerts, provider) =>
  provider ? alerts.filter((a) => a.provider.toLowerCase() === provider.toLowerCase()) : [];

/** The first model a tool call's input names on a low account. */
export function lowModelIn(input, alerts, depth = 0) {
  if (!alerts.length || depth > 5 || !input || typeof input !== "object") return undefined;
  for (const [key, value] of Object.entries(input)) {
    if (/model/i.test(key))
      for (const v of Array.isArray(value) ? value : [value]) if (alertsOn(alerts, providerOf(v)).length) return v;
    if (value && typeof value === "object") {
      const hit = lowModelIn(value, alerts, depth + 1);
      if (hit) return hit;
    }
  }
  return undefined;
}

// Tools that start new work: sub-agents, workflows, delegations, sessions,
// an advisor consultation.
export const NEW_WORK_TOOLS = new Set(["subagent_start", "workflow", "delegate", "delegate_resume",
  "zeron_create_chat", "zeron_create_chats", "advisor"]);
const describeAlert = (a) =>
  `${a.provider} account ${a.accountId}: ${a.window} window ${Number(a.remainingPct).toFixed(1)}% left` +
  (a.resetsAt ? `, resets ${new Date(a.resetsAt).toISOString().slice(0, 16).replace("T", " ")} UTC` : "");
export function usageBlock(event, alerts, ownModel) {
  if (!alerts.length || !NEW_WORK_TOOLS.has(event.toolName)) return undefined;
  const own = alertsOn(alerts, ownModel?.provider);
  if (own.length)
    return `${USAGE_MARK} Blocked ${event.toolName}: the subscription this session runs on is almost used up ` +
      `(${own.map(describeAlert).join("; ")}). Start no new work. Finish the step you are on, write your handoff and end your turn.`;
  const model = lowModelIn(event.input, alerts);
  if (model)
    return `${USAGE_MARK} Blocked ${event.toolName}: ${model} runs on a subscription that is almost used up ` +
      `(${alertsOn(alerts, providerOf(model)).map(describeAlert).join("; ")}). Use a model from another provider; other accounts stay usable.`;
  return undefined;
}

// Global helpers answer one question; they are told to keep it short, not to
// write a handoff (their caller does).
const HELPER_SURFACES = new Set(["advisor", "decisions", "jev", "memory", "btw"]);
export function usageNote(alerts, { surface = "", now = Date.now() } = {}) {
  const lines = alerts.map((a) => `- ${describeAlert(a)}`).join("\n");
  if (HELPER_SURFACES.has(surface))
    return `${USAGE_MARK} The subscription you run on is almost used up:\n${lines}\n` +
      `Answer what you were asked as briefly as you can and stop; start nothing new.`;
  const stamp = new Date(now).toISOString().replace(/[-:]/g, "").slice(0, 13) + "Z";
  const path = `.pi/handoffs/usage-${alerts[0]?.provider ?? "account"}-${stamp}.md`;
  return `${USAGE_MARK} The subscription this session runs on is almost used up:\n${lines}\n` +
    `Finish the step you are on now, as quickly as is safe: do not abandon a half-done action that would leave ` +
    `things broken (finish it or roll it back), but start nothing new: no new sub-agents, workflows, delegations ` +
    `or sessions on this account. Then write a handoff to \`${path}\` in your working directory (goal, what is done, ` +
    `what is in progress and its exact state, next steps, branches/commits/files, commands to verify), tell any ` +
    `sub-agents you started to wrap up the same way, and end your turn with the handoff path in your final answer.`;
}

/**
 * Tell this Pi once per alert key. A running Pi below the session gets a
 * steer at once; the Zeron session itself is told by the engine while it
 * works, so here it (and any idle Pi) gets the note when its next turn starts.
 */
export function usageWatcher(pi, { depth, env = process.env, intervalMs = 15000, read = () => usageAlerts(env) } = {}) {
  const told = new Set();
  let model, timer, ctxRef;
  const fresh = () => {
    const own = alertsOn(read(), model?.provider);
    return own.filter((a) => !told.has(a.key));
  };
  const note = (alerts) => usageNote(alerts, { surface: env.PI_USAGE_SURFACE ?? "" });
  const check = () => {
    if (depth === 0 || !ctxRef) return false;
    let idle = true;
    try { idle = ctxRef.isIdle(); } catch {}
    if (idle) return false;
    const alerts = fresh();
    if (!alerts.length) return false;
    for (const a of alerts) told.add(a.key);
    try { pi.sendUserMessage(note(alerts), { deliverAs: "steer" }); } catch {}
    return true;
  };
  pi.on("session_start", (_event, ctx) => {
    ctxRef = ctx; model = ctx?.model ?? model;
    if (!timer && intervalMs > 0) { timer = setInterval(check, intervalMs); timer.unref?.(); }
  });
  pi.on("model_select", (event) => { model = event?.model ?? model; });
  pi.on("session_shutdown", () => { if (timer) clearInterval(timer); timer = undefined; });
  pi.on("turn_start", () => { check(); });
  pi.on("before_agent_start", (_event, ctx) => {
    ctxRef = ctx ?? ctxRef; model = ctx?.model ?? model;
    const alerts = fresh();
    if (!alerts.length) return undefined;
    for (const a of alerts) told.add(a.key);
    return { message: { customType: "zeron-usage-alert", content: note(alerts), display: true } };
  });
  return { check, told, model: () => model };
}

// ---- the tree policy. The rules the user set for the orchestrator tree this
// Pi belongs to, in the file `ZERON_TREE_POLICY` names (the engine keeps it
// current): builds and tests run on GitHub only, how many heavy commands run
// at once, the models the tree may use, and whether the spending cap halted
// it. Every Pi of the tree (sub-agents and workflows too) inherits the env
// and this guard, so none of them gets around it. Re-read on change, so an
// edit holds from the next call. Global helpers keep their own rules. Unknown
// (no env, no file, a torn file) blocks nothing. Like the rest of the guard
// this stops mistakes, it is not a sandbox: a script, a make target or an
// interpreter running a build is not seen.
export const TREE_MARK = "[zeron:tree-policy";
let treeSeen = { file: "", mtimeMs: -1, size: -1, policy: undefined };
export function treePolicy(env = process.env, fs = { statSync, readFileSync }) {
  const file = env.ZERON_TREE_POLICY;
  if (!file || policyExempt(env)) return undefined;
  const last = () => (treeSeen.file === file ? treeSeen.policy : undefined);
  let st;
  try { st = fs.statSync(file); }
  catch (e) {
    // Gone: nothing set. Transient I/O (a Windows rename-over): the last read.
    if (e?.code === "ENOENT") { treeSeen = { file: "", mtimeMs: -1, size: -1, policy: undefined }; return undefined; }
    return last();
  }
  if (treeSeen.file === file && treeSeen.mtimeMs === st.mtimeMs && treeSeen.size === st.size) return treeSeen.policy;
  try {
    const raw = JSON.parse(fs.readFileSync(file, "utf8"));
    const limit = Number(raw?.heavyCommandLimit);
    const policy = {
      githubOnlyTests: raw?.githubOnlyTests === true,
      heavyCommandLimit: Number.isInteger(limit) && limit > 0 ? limit : 0,
      allowedModels: Array.isArray(raw?.allowedModels)
        ? raw.allowedModels.filter((m) => typeof m === "string" && m.trim()).map((m) => m.trim()) : [],
      halted: raw?.halted && typeof raw.halted === "object" ? { reason: String(raw.halted.reason ?? "") } : undefined,
    };
    treeSeen = { file, mtimeMs: st.mtimeMs, size: st.size, policy };
    return policy;
  } catch (e) {
    if (e instanceof SyntaxError) { treeSeen = { file, mtimeMs: st.mtimeMs, size: st.size, policy: undefined }; return undefined; }
    return last();
  }
}
/** The models the tree may use; empty allows any. */
export const allowedModels = (env = process.env) => treePolicy(env)?.allowedModels ?? [];

// Heavy commands: compilers, bundlers and test runners.
const HEAVY_WRAPPERS = new Set([...RO_WRAPPERS, "sudo", "doas", "setsid", "exec"]);
const HEAVY_DIRECT = new Set(["rustc", "rustdoc", "tsc", "vitest", "jest", "mocha", "webpack", "pytest", "py.test", "tox", "nox",
  "gradle", "gradlew", "mvn", "mvnw", "ant", "make", "gmake", "cmake", "ninja", "msbuild", "xcodebuild", "gcc", "g++", "cc", "c++",
  "clang", "clang++", "cl", "javac", "kotlinc", "swiftc", "bazel", "bazelisk", "buck2", "sbt"]);
const HEAVY_SUB = {
  cargo: new Set(["build", "b", "check", "c", "test", "t", "clippy", "doc", "d", "bench", "run", "r", "nextest", "llvm-cov", "install",
    "fix", "miri", "tarpaulin", "rustc", "rustdoc", "package"]),
  dotnet: new Set(["build", "test", "publish", "pack", "msbuild", "run"]),
  go: new Set(["build", "test", "vet", "install", "run"]),
  deno: new Set(["test", "check", "compile", "bench"]),
  zig: new Set(["build", "test"]),
  vite: new Set(["build"]), next: new Set(["build"]), nuxt: new Set(["build"]),
  nx: new Set(["build", "test", "lint", "e2e", "affected"]), turbo: new Set(["run", "build", "test", "lint"]),
};
const NODE_MANAGERS = new Set(["npm", "pnpm", "yarn", "bun"]);
const NODE_RUNNERS = new Set(["npx", "pnpx", "bunx"]);
const NODE_TOOLS = new Set(["tsc", "vitest", "jest", "mocha", "webpack"]);
const NODE_DIRECT = new Set(["build", "test", "t", "tst", "typecheck", "lint", "check", "compile"]);
const HEAVY_SCRIPT = /^(build|test|tests|typecheck|type-check|lint|check|compile|e2e|ci|coverage|bundle)(:.*|-.*)?$/i;
const FLAG_VALUED = new Set(["--filter", "-F", "--workspace", "--prefix", "--cwd", "--dir", "-C", "--manifest-path"]);
const PY_MODULES = new Set(["pytest", "unittest", "tox", "nox", "mypy"]);

/** The arguments that are not flags (and not the value of one that takes a value). */
function positional(args) {
  const out = [];
  for (let k = 0; k < args.length; k++) {
    const a = args[k];
    if (a === "--") break;
    if (FLAG_VALUED.has(a)) { k++; continue; }
    if (!a.startsWith("-")) out.push(a);
  }
  return out;
}

function nodeHeavy(name, args) {
  const [first, second] = positional(args);
  if (!first) return undefined;
  if (name === "npm" ? /^(test|t|tst)$/.test(first) : NODE_DIRECT.has(first)) return { name: `${name} ${first}` };
  if (/^(run|run-script|rum|urn)$/.test(first) && second && HEAVY_SCRIPT.test(second)) return { name: `${name} run ${second}` };
  return undefined;
}

function heavySegment(words, dialect, depth) {
  if (depth > 8) return undefined;
  let i = 0;
  while (i < words.length && /^[A-Za-z_]\w*=/.test(words[i].raw)) i++;
  if (dialect === "ps") while (words[i] && (words[i].cooked === "&" || words[i].cooked === ".")) i++;
  const head = words[i];
  if (!head || head.dyn) return undefined;
  const name = nameOf(head), rest = words.slice(i + 1), args = rest.map((w) => w.cooked);
  if (HEAVY_WRAPPERS.has(name)) {
    let k = 0;
    while (k < args.length && (/^-/.test(args[k]) || /^[A-Za-z_]\w*=/.test(rest[k].raw) || /^\d+[smhd]?$/.test(args[k]))) k++;
    return k < rest.length ? heavySegment(rest.slice(k), dialect, depth + 1) : undefined;
  }
  if (SHELLS.test(name)) {
    const k = args.findIndex((a) => /^-[a-z]*c[a-z]*$/.test(a));
    return k >= 0 && args[k + 1] ? heavyCommand(args[k + 1], "posix", depth + 1) : undefined;
  }
  if (name === "powershell" || name === "pwsh") {
    const k = args.findIndex((a) => /^-(c|command|commandwithargs)$/i.test(a));
    return k >= 0 && args[k + 1] ? heavyCommand(args.slice(k + 1).join(" "), "ps", depth + 1) : undefined;
  }
  if (name === "cmd") {
    const k = args.findIndex((a) => /^\/[ck]$/i.test(a));
    return k >= 0 ? heavyCommand(args.slice(k + 1).join(" "), "cmd", depth + 1) : undefined;
  }
  if (HEAVY_DIRECT.has(name)) return { name };
  if (NODE_MANAGERS.has(name)) {
    const found = nodeHeavy(name, args);
    if (found) return found;
  }
  if (NODE_RUNNERS.has(name) || ((name === "pnpm" || name === "yarn") && args[0] === "dlx") || (name === "npm" && args[0] === "exec")) {
    const tool = positional(args.filter((a, k) => !(k === 0 && /^(dlx|exec)$/.test(a))))[0];
    const bare = tool?.replace(/@[^/]*$/, "");
    if (bare && NODE_TOOLS.has(bare)) return { name: `${name} ${bare}` };
  }
  if (/^(python[\d.]*|py|pypy3?)$/.test(name)) {
    const m = args.indexOf("-m");
    if (m >= 0 && PY_MODULES.has(args[m + 1])) return { name: `${name} -m ${args[m + 1]}` };
  }
  const sub = HEAVY_SUB[name];
  if (sub) {
    const verb = positional(args).find((a) => !a.startsWith("+"));
    if (verb && sub.has(verb)) return { name: `${name} ${verb}` };
  }
  return undefined;
}

/** The heavy command a shell line runs ({ name, seg }), looking through
 * wrappers (`rtk`, `timeout`, `env`, `nohup`), nested shells and command
 * substitutions. `cargo fmt`, git, gh and searches are not heavy. */
export function heavyCommand(src, dialect = "posix", depth = 0) {
  if (depth > 8 || typeof src !== "string" || src.length > 262144) return undefined;
  let parsed;
  try { parsed = split(src, dialect); } catch { return undefined; }
  for (const words of parsed.segs) {
    const found = heavySegment(words, dialect, depth);
    if (found) return { ...found, seg: found.seg ?? words.map((w) => w.raw).join(" ") };
  }
  for (const n of parsed.nested) {
    const found = heavyCommand(n.src, n.dialect, depth + 1);
    if (found) return found;
  }
  return undefined;
}

const SHELL_LIKE = /(bash|shell|exec|terminal|run_command|powershell|pwsh)/i;
/** The shell line a tool call runs: Pi's `bash` and `powershell`, and any
 * shell-like tool (the user's `bash_bg`) with a `command`. */
export function shellCommandOf(event) {
  const command = event?.input?.command;
  const name = String(event?.toolName ?? "");
  if (typeof command !== "string" || !command.trim()) return undefined;
  if (!SHELL_TOOLS.has(name) && !SHELL_LIKE.test(name)) return undefined;
  return { command, dialect: /powershell|pwsh/i.test(name) ? "ps" : "posix" };
}

// What a halted tree may still do: read. It starts nothing and changes nothing.
const HALT_EXTRA = new Set(["zeron_send_message", "zeron_send_messages"]);

/** Why the tree policy blocks this tool call, or undefined. The checks that
 * need no waiting; the heavy-command limit is `heavySlot`. */
export function treeBlock(event, policy, annotations) {
  if (!policy) return undefined;
  const tool = String(event?.toolName ?? "");
  if (policy.halted) {
    let writes;
    try { writes = classifyReadOnly({ toolName: tool, input: event.input ?? {}, annotations }); }
    catch { writes = hit("ro-error", "the check could not analyse this call"); }
    if (NEW_WORK_TOOLS.has(tool) || HALT_EXTRA.has(tool) || writes)
      return `${TREE_MARK} rule=halted] Blocked ${tool}: this orchestrator's tree is paused (${policy.halted.reason || "its spending cap"}). ` +
        `Start nothing and change nothing: finish the step you are on safely, say where things stand in your final reply and end your turn.`;
  }
  if (policy.githubOnlyTests) {
    const sh = shellCommandOf(event);
    const heavy = sh && heavyCommand(sh.command, sh.dialect);
    if (heavy)
      return `${TREE_MARK} rule=github-only-tests] Blocked: ${heavy.name} would build or test on this machine, and this orchestrator runs builds ` +
        `and tests on GitHub only. Push your branch, open a draft pull request and read CI with \`gh pr checks\` or \`gh run watch\`. ` +
        `Do not retry or rephrase it. If CI is not an option, stop and tell your parent.`;
  }
  if (policy.allowedModels.length && !POLICY_TOOLS_EXEMPT.has(tool)) {
    const model = disallowedModelIn(event.input, policy.allowedModels);
    if (model)
      return `${TREE_MARK} rule=allowed-models] Blocked ${tool}: model ${model} is not allowed in this orchestrator; only these are: ` +
        `${policy.allowedModels.join(", ")}. Use one of them.`;
  }
  return undefined;
}

// The heavy-command limit: a slot is a file in `<policy file>.slots`, named
// `<pid>-<tool call>`, so every Pi of the tree on this device counts the
// same ones. A slot of a dead process, or older than half an hour, is
// reaped; two Pis that count at once settle it by the order the files were
// made, and the later one gives its file back.
const SLOT_TTL_MS = 30 * 60 * 1000;
const pidAlive = (pid) => { try { process.kill(pid, 0); return true; } catch (e) { return e?.code === "EPERM"; } };
export function slotsDir(env = process.env) {
  const file = env.ZERON_TREE_POLICY;
  return file ? file.replace(/\.json$/i, "") + ".slots" : undefined;
}
/** Take one of `limit` slots in `dir` for `id`, or undefined when all are held. */
export function takeSlot(dir, limit, id, { pid = process.pid, now = Date.now(), alive = pidAlive } = {}) {
  mkdirSync(dir, { recursive: true });
  const held = () => {
    const out = [];
    for (const name of readdirSync(dir)) {
      const owner = /^(\d+)-/.exec(name);
      if (!owner) continue;
      const file = join(dir, name);
      let st;
      try { st = statSync(file); } catch { continue; }
      if (now - st.mtimeMs > SLOT_TTL_MS || !alive(Number(owner[1]))) { try { unlinkSync(file); } catch {} continue; }
      out.push({ name, mtimeMs: st.mtimeMs });
    }
    return out.sort((a, b) => a.mtimeMs - b.mtimeMs || (a.name < b.name ? -1 : 1));
  };
  if (held().length >= limit) return undefined;
  const name = `${pid}-${String(id).replace(/[^\w.-]/g, "_")}`;
  const file = join(dir, name);
  try { writeFileSync(file, String(now), { flag: "wx" }); } catch (e) { if (e?.code !== "EEXIST") throw e; }
  const rank = held().findIndex((slot) => slot.name === name);
  if (rank < 0 || rank >= limit) { try { unlinkSync(file); } catch {} return undefined; }
  return file;
}
export function dropSlot(file) { try { unlinkSync(file); } catch {} }

/** Wait for a slot: `{ file }` when taken, `{ busy: true }` when none freed in
 * time, `{}` when the slots cannot be used at all (then nothing is held back). */
export async function waitForSlot(dir, limit, id, signal, { pollMs = 500, waitMs = Number(process.env.ZERON_TREE_SLOT_WAIT_MS) || 90000 } = {}) {
  const until = Date.now() + waitMs;
  for (;;) {
    let file;
    try { file = takeSlot(dir, limit, id); } catch { return {}; }
    if (file) return { file };
    if (signal?.aborted || Date.now() >= until) return { busy: true };
    await new Promise((resolve) => setTimeout(resolve, pollMs + Math.floor(Math.random() * 200)));
  }
}

export default function deleteGuard(pi) {
  inheritIntoChildren();
  guardCompiledChildren();
  const depth = piDepth(process.env, process.pid);
  usageWatcher(pi, { depth });
  process.env.ZERON_PI_PID = String(process.pid);          // for the Pis this one starts
  process.env.ZERON_SUBAGENT_DEPTH = String(depth);
  const policed = Boolean(process.env.ZERON_MODEL_POLICY || process.env.ZERON_TREE_POLICY) && !policyExempt();
  if (policed) enforceModels(pi);
  // Heavy-command slots this Pi holds, by tool call; freed when the call ends.
  const slots = new Map();
  const release = (id) => {
    const file = slots.get(id);
    if (file) { slots.delete(id); dropSlot(file); }
  };
  const releaseAll = () => { for (const id of [...slots.keys()]) release(id); };
  pi.on("tool_result", (event) => { release(event?.toolCallId); });
  pi.on("tool_execution_end", (event) => { release(event?.toolCallId); });
  pi.on("agent_end", releaseAll);
  pi.on("session_shutdown", releaseAll);
  pi.on("tool_call", (event, ctx) => {            // also fires for nested codemode calls (parentToolCallId)
    {
      let reason;
      try { reason = usageBlock(event, usageAlerts(), ctx?.model); } catch {}
      if (reason) return { block: true, reason };
    }
    if (policed && !POLICY_TOOLS_EXEMPT.has(event.toolName)) {
      let hit;
      try { hit = disabledModelIn(event.input, disabledModels()); } catch {}
      if (hit) return { block: true, reason: MODEL_REASON(hit) };
    }
    const annotationsOf = () => {
      try { return pi.getAllTools().find((t) => t.name === event.toolName)?.annotations; } catch { return undefined; }
    };
    if (readOnlyOn()) {
      let r;
      try { r = classifyReadOnly({ toolName: event.toolName, input: event.input ?? {}, annotations: annotationsOf() }); }
      catch { r = hit("ro-error", "the read-only check could not analyse this call"); }
      if (r) return { block: true, reason: READ_ONLY_REASON(r) };
    }
    let tree;
    try { tree = treePolicy(); } catch {}
    if (tree) {
      let reason;
      try { reason = treeBlock(event, tree, annotationsOf()); } catch {}
      if (reason) return { block: true, reason };
    }
    if (guardOn()) {
      let h;
      try {
        const annotations = annotationsOf();     // name rules still apply without it
        h = decide(event, annotations, ctx?.cwd);
      } catch {
        h = SHELL_TOOLS.has(event.toolName) ? hit("guard-error", "the guard could not analyse this command") : undefined;
      }
      if (h) return { block: true, reason: REASON(h) };
    }
    // Last, so a call blocked above never holds a slot. The wait is the only
    // time this handler answers late.
    if (tree?.heavyCommandLimit && event.toolCallId) {
      let sh, heavy;
      try { sh = shellCommandOf(event); heavy = sh && heavyCommand(sh.command, sh.dialect); } catch {}
      const dir = slotsDir();
      if (heavy && dir) {
        return waitForSlot(dir, tree.heavyCommandLimit, event.toolCallId, ctx?.signal).then(({ file, busy }) => {
          if (file) { slots.set(event.toolCallId, file); return undefined; }
          if (!busy) return undefined;
          return { block: true, reason: `${TREE_MARK} rule=heavy-command-limit] Blocked: ${heavy.name} would be heavy command number ` +
            `${tree.heavyCommandLimit + 1} in this orchestrator's tree, which allows ${tree.heavyCommandLimit} at once, and none finished in time. ` +
            `Do not retry in a loop: do other work first, or push and read CI with gh instead.` };
        });
      }
    }
  });
  if (depth === 0 && process.env.ZERON_PACKAGE_CHECK === "1") {
    delete process.env.ZERON_PACKAGE_CHECK;     // once: not on a reload, not in children
    pi.on("session_start", (event, ctx) => {
      if (event?.reason !== "startup") return;
      packageUpdates(ctx.cwd).then((packages) => {
        if (packages?.length) ctx.ui.notify(PACKAGE_UPDATES_PREFIX + JSON.stringify({ v: 1, packages }), "info");
      }, () => {});
    });
  }
  // Pi's /reload lives in its terminal; in RPC mode only an extension's
  // command can run it. The Zeron session gets it, not its sub-agents.
  if (depth === 0 && typeof pi.registerCommand === "function") pi.registerCommand("reload", {
    description: RELOAD_DESCRIPTION,
    handler: async (_args, ctx) => { await ctx.reload(); },
  });
  // The user's own `!cmd` (user_bash) is deliberately not hooked.
}
