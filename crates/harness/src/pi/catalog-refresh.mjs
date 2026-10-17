// Zeron's model probe loads this, and only the probe: it refreshes every
// provider's catalog before Zeron reads `get_available_models`. Pi's RPC mode
// starts that refresh in the background and answers from the stored catalog,
// so a model a provider just added would otherwise reach Zeron's picker one
// start late, or (inside the pi.dev catalog's 4 h window) hours late.
// `force` skips that window; Zeron asks for it on its first probe after it
// opens. A refresh that fails or times out leaves the stored catalog.
// Afterwards it names every available model's provider the way Pi does
// (`ZERON-PROVIDERS {"names":{"opencode-go":"OpenCode Go"}}`), so the picker
// groups by provider name for providers Zeron has never heard of too.
export const REFRESH_COMMAND = "pi_ui_refresh_models";
export const REFRESH_TIMEOUT_MS = 20_000;
export const PROVIDERS_PREFIX = "ZERON-PROVIDERS ";

export function providerNames(registry) {
  const names = {};
  if (typeof registry?.getAvailable !== "function") return names;
  for (const model of registry.getAvailable()) {
    const id = model?.provider;
    if (typeof id !== "string" || id in names) continue;
    let name;
    try {
      name = registry.getProviderDisplayName?.(id);
    } catch {
      // A provider without a name keeps its id.
    }
    names[id] = typeof name === "string" && name.trim() ? name.trim() : id;
  }
  return names;
}

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
      ctx.ui?.notify?.(
        PROVIDERS_PREFIX + JSON.stringify({ names: providerNames(ctx.modelRegistry) }),
        "info",
      );
    },
  });
}
