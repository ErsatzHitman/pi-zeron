// Zeron delete guard. Blocks tool calls that delete files, git refs or remote
// resources, in every Pi run Zeron starts and every child that inherits it.
// A guard against mistakes and model-driven deletes, not a sandbox.
import { readFileSync, statSync } from "node:fs";
import { basename, resolve } from "node:path";
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
const READONLY_TOOLS = new Set(["read", "grep", "find", "ls"]);
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
export default function deleteGuard(pi) {
  inheritIntoChildren();
  pi.on("tool_call", (event, ctx) => {            // also fires for nested codemode calls (parentToolCallId)
    if (!guardOn()) return;
    let h;
    try {
      let annotations; try { annotations = pi.getAllTools().find((t) => t.name === event.toolName)?.annotations; } catch { /* name rules still apply */ }
      h = decide(event, annotations, ctx?.cwd);
    } catch {
      h = SHELL_TOOLS.has(event.toolName) ? hit("guard-error", "the guard could not analyse this command") : undefined;
    }
    if (h) return { block: true, reason: REASON(h) };
  });
  // The user's own `!cmd` (user_bash) is deliberately not hooked.
}
