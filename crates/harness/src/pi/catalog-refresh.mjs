// Zeron's model probe loads this, and only the probe: it refreshes every
// provider's catalog before Zeron reads `get_available_models`. Pi's RPC mode
// starts that refresh in the background and answers from the stored catalog,
// so a model a provider just added would otherwise reach Zeron's picker one
// start late, or (inside the pi.dev catalog's 4 h window) hours late.
// `force` skips that window; Zeron asks for it on its first probe after it
// opens. A refresh that fails or times out leaves the stored catalog.
export const REFRESH_COMMAND = "pi_ui_refresh_models";
export const REFRESH_TIMEOUT_MS = 20_000;

export async function refreshModels(registry, force, env = process.env) {
  if (typeof registry?.refresh !== "function") return;
  const signal = AbortSignal.timeout(REFRESH_TIMEOUT_MS);
  try {
    await registry.refresh({ allowNetwork: env.PI_OFFLINE === undefined, force, signal });
  } catch {
    // Pi records refresh errors itself; the stored catalog stays usable.
  }
}

export default function zeronCatalogRefresh(pi) {
  pi.registerCommand(REFRESH_COMMAND, {
    description: "Refresh model catalogs for Zeron's model picker",
    handler: async (args, ctx) => {
      await refreshModels(ctx.modelRegistry, String(args ?? "").trim() === "force");
    },
  });
}
