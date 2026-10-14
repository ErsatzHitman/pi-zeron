// Zeron delete guard, child side. A guarded Pi adds this file to its node
// flags (guard.mjs `inheritIntoChildren`), and Pi's child launchers (fleet
// subagents, loop workers, the advisor) pass those flags on, so every Pi it
// starts loads this first and starts with the guard extension too. Only the
// Pi script itself is touched; any other node program is left alone. A guard
// file that has gone missing fails the child closed.
import { existsSync } from "node:fs";
import { resolve } from "node:path";

export function addGuard(argv, env, exists = existsSync) {
  const ext = env.ZERON_DELETE_GUARD_EXT, pi = env.ZERON_DELETE_GUARD_PI;
  if (!ext || !pi || !argv[1] || resolve(argv[1]) !== resolve(pi)) return "skip";
  if (!exists(ext)) return "missing";
  if (argv.some((arg, i) => arg === ext && (argv[i - 1] === "-e" || argv[i - 1] === "--extension"))) return "present";
  argv.splice(2, 0, "--extension", ext);
  return "added";
}

if (addGuard(process.argv, process.env) === "missing") {
  process.stderr.write(`Zeron delete guard extension is missing: ${process.env.ZERON_DELETE_GUARD_EXT}\n`);
  process.exit(1);
}
