// Per-run Zeron delegation bridge. No global settings or packages are modified.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

// A tool call outlives the longest wait it asks for (wait_for_turn, or any
// request of a batch), with 11 minutes as the floor.
export function callTimeoutMs(method, params) {
  if (method !== "tools/call") return 15_000;
  const args = params?.arguments ?? {};
  const requests = Array.isArray(args.requests) ? args.requests : [];
  const secs = [args.timeout_secs, ...requests.map(r => r?.timeout_secs)]
    .map(Number).filter(Number.isFinite);
  return Math.max(660_000, (secs.length ? Math.max(...secs) : 0) * 1000 + 30_000);
}

// Zeron tools a plain chat rarely needs, with what each is for. They are
// deferred: not declared on every request until tool_search loads them,
// and named in a short prompt section so the model knows they exist. An
// orchestrator's root and members keep every tool declared.
const ON_DEMAND = {
  whoami: "this chat, device and place in a tree",
  list_devices: "devices Zeron can run chats on",
  list_projects: "projects on a device",
  register_project: "add a folder as a project",
  github_preflight: "check GitHub access before GitHub work",
  list_harnesses: "agent harnesses on a device",
  list_models: "models a harness can run",
  archive_chat: "hide or restore a chat",
  list_subsessions: "what a chat delegated",
  inspect_subsession: "one delegated run in detail",
};

export function onDemandSection(name, deferred) {
  if (!deferred.length) return "";
  return `More Zeron tools load when needed: call tool_search (for example "zeron ${deferred[0]}") to load one, then call it.\n`
    + deferred.map(tool => `- ${name}_${tool}: ${ON_DEMAND[tool]}`).join("\n");
}

export default function (pi) {
  const config = JSON.parse(process.env.ZERON_PI_MCP);
  const plainChat = !process.env.ZERON_ROLE;
  let deferred = [];
  let child, lines, nextId = 0;
  const pending = new Map();
  function fail(error) {
    for (const request of [...pending.values()]) request.reject(error);
  }
  function close() {
    fail(new Error("Zeron MCP connection closed"));
    lines?.close();
    child?.kill();
    child = undefined;
  }
  function rpc(method, params, signal) {
    if (!child?.stdin.writable) return Promise.reject(new Error("Zeron MCP is not connected"));
    if (signal?.aborted) return Promise.reject(new Error("Zeron MCP request cancelled"));
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      const cleanup = () => {
        clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
        pending.delete(id);
      };
      const abort = () => {
        child?.stdin.write(JSON.stringify({jsonrpc: "2.0", method: "notifications/cancelled", params: {requestId: id}}) + "\n");
        cleanup();
        reject(new Error("Zeron MCP request cancelled"));
      };
      const timer = setTimeout(() => {
        child?.stdin.write(JSON.stringify({jsonrpc: "2.0", method: "notifications/cancelled", params: {requestId: id}}) + "\n");
        cleanup();
        reject(new Error(`Zeron MCP ${method} timed out`));
      }, callTimeoutMs(method, params));
      pending.set(id, {
        resolve: value => { cleanup(); resolve(value); },
        reject: error => { cleanup(); reject(error); },
      });
      signal?.addEventListener("abort", abort, {once: true});
      child.stdin.write(JSON.stringify({jsonrpc: "2.0", id, method, params}) + "\n", error => {
        if (error) pending.get(id)?.reject(error);
      });
    });
  }
  pi.on("session_start", async () => {
    close();
    child = spawn(config.command, config.args, {
      env: {...process.env, ...config.env}, stdio: ["pipe", "pipe", "inherit"],
    });
    const owned = child;
    const failed = error => { if (child === owned) fail(error); };
    child.on("error", failed);
    child.on("exit", () => failed(new Error("Zeron MCP server exited")));
    child.stdin.on("error", failed);
    lines = createInterface({input: child.stdout});
    lines.on("line", line => {
      let message;
      try { message = JSON.parse(line); } catch { return; }
      const request = pending.get(message.id);
      if (!request) return;
      if (message.error) request.reject(new Error(message.error.message));
      else request.resolve(message.result);
    });
    try {
      await rpc("initialize", {protocolVersion: "2024-11-05", capabilities: {}, clientInfo: {name: "zeron-pi", version: "1"}});
      child.stdin.write(JSON.stringify({jsonrpc: "2.0", method: "notifications/initialized"}) + "\n");
      const {tools} = await rpc("tools/list", {});
      deferred = [];
      for (const tool of tools) {
        const onDemand = plainChat && Object.hasOwn(ON_DEMAND, tool.name);
        if (onDemand) deferred.push(tool.name);
        pi.registerTool({
          name: `${config.name}_${tool.name}`, label: `Zeron: ${tool.name}`,
          description: tool.description, parameters: tool.inputSchema,
          ...(onDemand ? {exposure: "deferred"} : {}),
          async execute(_id, args, signal) {
            const result = await rpc("tools/call", {name: tool.name, arguments: args}, signal);
            if (result.isError) throw new Error(result.content?.filter(c => c.type === "text").map(c => c.text).join("\n") || "Zeron tool failed");
            return {content: result.content, details: result.structuredContent ?? {}};
          },
        });
      }
      const active = pi.getActiveTools();
      if (deferred.length && !active.includes("tool_search")
        && pi.getAllTools().some(tool => tool.name === "tool_search")) {
        pi.setActiveTools([...active, "tool_search"]);
      }
    } catch (error) {
      close();
      throw error;
    }
  });
  pi.on("before_agent_start", event => {
    // Name only the tools this session can load: a `--tools` allowlist (a btw
    // chat) registers neither them nor tool_search, and a loaded one is declared.
    const registered = new Set(pi.getAllTools().map(tool => tool.name));
    const active = new Set(pi.getActiveTools());
    const loadable = active.has("tool_search")
      ? deferred.filter(tool => registered.has(`${config.name}_${tool}`) && !active.has(`${config.name}_${tool}`))
      : [];
    const text = onDemandSection(config.name, loadable);
    if (!text) return;
    const options = event.systemPromptOptions;
    if (options && options.forceSystemPrompt === undefined) {
      options.sections ??= {};
      options.sections.zeron_tools = text;
      return;
    }
    return {systemPrompt: `${event.systemPrompt ?? ""}\n\n<zeron_tools>\n${text}\n</zeron_tools>`};
  });
  pi.on("session_shutdown", close);
  process.once("exit", close);
}
