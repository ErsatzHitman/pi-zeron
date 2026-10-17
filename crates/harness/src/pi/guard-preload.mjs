// Zeron delete guard, child side. A guarded Pi adds this file to its node
// flags (guard.mjs `inheritIntoChildren`), and Pi's child launchers (fleet
// subagents, loop workers, herdr delegates, the advisor) pass those flags on,
// so every Pi it starts loads this first and starts with the guard extension
// too. Zeron writes the guard's paths into CONFIG at install, so a child
// started without the parent's env (a command typed into a herdr pane or a
// new terminal tab) is covered too; this file then restores that env for the
// child and its own children. Only the Pi script is touched (the parent's,
// when known; else Pi's `cli` entry); any other node program is left alone.
// A guard file that has gone missing fails the child closed.
import { existsSync } from "node:fs";
import { basename, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const CONFIG = {}; // zeron:guard-config

const PI_ENTRY = /^cli\.(c|m)?[jt]s$/i;

export function addGuard(argv, env, exists = existsSync, config = CONFIG, self = undefined) {
  const ext = env.ZERON_DELETE_GUARD_EXT || config.ext;
  if (!ext || !argv[1]) return "skip";
  const pi = env.ZERON_DELETE_GUARD_PI;
  if (pi ? resolve(argv[1]) !== resolve(pi) : !PI_ENTRY.test(basename(argv[1]))) return "skip";
  env.ZERON_DELETE_GUARD_EXT ||= ext;
  if (config.file && !env.ZERON_DELETE_GUARD_FILE) env.ZERON_DELETE_GUARD_FILE = config.file;
  if (config.force && !env.ZERON_DELETE_GUARD_FORCE) env.ZERON_DELETE_GUARD_FORCE = "1";
  if (self && !env.ZERON_DELETE_GUARD_PRELOAD) env.ZERON_DELETE_GUARD_PRELOAD = self;
  if (config.usage && !env.ZERON_USAGE_ALERTS_FILE) env.ZERON_USAGE_ALERTS_FILE = config.usage;
  if (!exists(ext)) return "missing";
  if (argv.some((arg, i) => arg === ext && (argv[i - 1] === "-e" || argv[i - 1] === "--extension"))) return "present";
  argv.splice(2, 0, "--extension", ext);
  return "added";
}

const outcome = addGuard(process.argv, process.env, existsSync, CONFIG, fileURLToPath(import.meta.url));
if (outcome === "missing") {
  process.stderr.write(`Zeron delete guard extension is missing: ${process.env.ZERON_DELETE_GUARD_EXT}\n`);
  process.exit(1);
}
