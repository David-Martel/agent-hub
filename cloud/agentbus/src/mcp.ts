/** Stateless, authenticated MCP bridge for the Worker's implemented REST subset.
 * No local-store, inbox-cursor or global claims authority is advertised here. */
type ObjectValue = Record<string, unknown>;
type Rule = { type: string; items?: Rule; enum?: string[]; minimum?: number; maximum?: number };
const string: Rule = { type: "string" };
const strings: Rule = { type: "array", items: string };
const boolean: Rule = { type: "boolean" };
const object: Rule = { type: "object" };
const window: Rule = { type: "integer", minimum: 1, maximum: 10080 };
const limit: Rule = { type: "integer", minimum: 1, maximum: 500 };
function tool(name: string, description: string, properties: Record<string, Rule>, required: string[] = []) {
  return { name, description, inputSchema: {
    type: "object", properties, additionalProperties: false,
    ...(required.length ? { required } : {}),
  } };
}

export const MCP_TOOLS = [
  tool("bus_health", "Check this cloud tier's health; this does not qualify the on-site hub.", {}),
  tool("post_message", "Post a message to the cloud bus with token-bound sender identity.", {
    sender: string, recipient: string, topic: string, body: string, tags: strings,
    thread_id: string, priority: { type: "string", enum: ["low", "normal", "high", "urgent"] },
    request_ack: boolean, reply_to: string, metadata: object,
    schema: { type: "string", enum: ["finding", "status", "benchmark"] },
  }, ["sender", "recipient", "topic", "body"]),
  tool("list_messages", "List retained cloud messages with the native message filters.", {
    agent: string, sender: string, topic: string, repo: string, session: string,
    tag: strings, thread_id: string, since_minutes: window, limit, include_broadcast: boolean,
  }),
  tool("ack_message", "Acknowledge a message with token-bound agent and recipient authorization.", {
    agent: string, message_id: string, body: string,
  }, ["agent", "message_id"]),
  tool("set_presence", "Update cloud presence with token-bound agent identity.", {
    agent: string, status: string, session_id: string, capabilities: strings,
    ttl_seconds: { type: "integer", minimum: 1, maximum: 86400 }, metadata: object,
  }, ["agent"]),
  tool("list_presence", "List active cloud presence records.", {}),
  tool("list_presence_history", "List retained cloud SQLite presence history.", {
    agent: string, since_minutes: window, limit,
  }),
  tool("knock_agent", "Send a durable cloud knock; cloud SSE delivery is not implemented.", {
    sender: string, recipient: string, body: string, thread_id: string, tags: strings, request_ack: boolean,
  }, ["sender", "recipient"]),
];

const HEADERS = { "content-type": "application/json", "cache-control": "no-store", "x-content-type-options": "nosniff" };
const VERSIONS = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const MAX_REQUEST_BYTES = 1_048_576;
type RpcId = string | number | null;
function reply(id: RpcId, result: unknown): Response {
  return new Response(JSON.stringify({ jsonrpc: "2.0", id, result }), { headers: HEADERS });
}
function error(id: RpcId, code: number, message: string, status = 200): Response {
  return new Response(JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }), { status, headers: HEADERS });
}
function isObject(value: unknown): value is ObjectValue {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function matches(value: unknown, rule: Rule): boolean {
  if (rule.type === "array") return Array.isArray(value) && value.every((item) => matches(item, rule.items!));
  if (rule.type === "object") return isObject(value);
  if (rule.type === "integer") return typeof value === "number" && Number.isSafeInteger(value)
    && value >= rule.minimum! && value <= rule.maximum!;
  return typeof value === rule.type && (!rule.enum || rule.enum.includes(value as string));
}
function validArguments(args: ObjectValue, definition: typeof MCP_TOOLS[number]): boolean {
  const { properties, required } = definition.inputSchema;
  return (required ?? []).every((key) => Object.hasOwn(args, key))
    && Object.entries(args).every(([key, value]) => Object.hasOwn(properties, key) && matches(value, properties[key]!));
}

/** Fixed paths only: no user-controlled URL or proxy to an external origin. */
function restCall(name: string, args: ObjectValue): { path: string; method: string; body?: ObjectValue } {
  const query = new URLSearchParams();
  const append = (target: string, value: unknown) => { if (value !== undefined) query.set(target, String(value)); };
  switch (name) {
    case "bus_health": return { path: "/health", method: "GET" };
    case "post_message": return { path: "/messages", method: "POST", body: args };
    case "ack_message": return { path: `/messages/${encodeURIComponent(args.message_id as string)}/ack`, method: "POST",
      body: { agent: args.agent, body: args.body ?? "ack" } };
    case "set_presence": return { path: `/presence/${encodeURIComponent(args.agent as string)}`, method: "PUT",
      body: { ...args, status: args.status ?? "online" } };
    case "list_presence": return { path: "/presence", method: "GET" };
    case "knock_agent": return { path: "/knock", method: "POST", body: args };
    case "list_presence_history":
      append("agent", args.agent); append("since", args.since_minutes ?? 1440); append("limit", args.limit ?? 50);
      return { path: `/presence/history?${query}`, method: "GET" };
    case "list_messages":
      for (const key of ["agent", "topic", "repo", "session", "thread_id"]) append(key, args[key]);
      append("from", args.sender); append("since", args.since_minutes ?? 1440); append("limit", args.limit ?? 50);
      append("broadcast", args.include_broadcast ?? true);
      for (const tag of (args.tag ?? []) as string[]) query.append("tag", tag);
      return { path: `/messages?${query}`, method: "GET" };
    default: throw new Error("unsupported tool");
  }
}

export async function handleMcp(request: Request, dispatch: (request: Request) => Promise<Response>): Promise<Response> {
  const origin = request.headers.get("origin");
  if (origin && origin !== new URL(request.url).origin) return error(null, -32600, "invalid origin", 403);
  if (request.method !== "POST") return new Response(null, { status: 405, headers: { ...HEADERS, allow: "POST" } });
  if (request.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase() !== "application/json") {
    return error(null, -32600, "application/json required", 415);
  }
  const version = request.headers.get("mcp-protocol-version") ?? "2025-03-26";
  if (!VERSIONS.includes(version)) return error(null, -32602, "unsupported protocol version", 400);
  let raw: unknown;
  try {
    const reader = request.body?.getReader();
    if (!reader) return error(null, -32700, "parse error", 400);
    const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });
    let bytes = 0, text = "";
    try {
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) break;
        bytes += chunk.value.byteLength;
        if (bytes > MAX_REQUEST_BYTES) { await reader.cancel(); return error(null, -32600, "request too large", 413); }
        text += decoder.decode(chunk.value, { stream: true });
      }
      text += decoder.decode();
    } finally { reader.releaseLock(); }
    raw = JSON.parse(text);
  } catch { return error(null, -32700, "parse error", 400); }
  if (!isObject(raw)) return error(null, -32600, "invalid request", 400);
  const hasId = Object.hasOwn(raw, "id");
  const validId = typeof raw.id === "string" || (typeof raw.id === "number" && Number.isSafeInteger(raw.id));
  const id: RpcId = hasId && validId ? raw.id as string | number : null;
  if (raw.jsonrpc !== "2.0" || typeof raw.method !== "string" || (hasId && !validId)
    || (raw.params !== undefined && !isObject(raw.params))) return error(id, -32600, "invalid request", 400);
  const params = (raw.params ?? {}) as ObjectValue;
  if (!hasId) {
    // Never execute a mutating tools/call notification without a result ID.
    if (raw.method !== "notifications/initialized" && raw.method !== "notifications/cancelled") {
      return error(null, -32600, "unsupported notification", 400);
    }
    return new Response(null, { status: 202, headers: HEADERS });
  }
  switch (raw.method) {
    case "initialize":
      if (typeof params.protocolVersion !== "string" || !isObject(params.capabilities)
        || !isObject(params.clientInfo) || typeof params.clientInfo.name !== "string"
        || typeof params.clientInfo.version !== "string") return error(id, -32602, "invalid initialize parameters");
      return reply(id, { protocolVersion: VERSIONS.includes(params.protocolVersion) ? params.protocolVersion : "2025-11-25",
        capabilities: { tools: { listChanged: false } }, serverInfo: { name: "agentbus-cloud", version: "0.2.0" } });
    case "ping": return reply(id, {});
    case "tools/list":
      if (params.cursor !== undefined) return error(id, -32602, "catalog does not use pagination cursors");
      return reply(id, { tools: MCP_TOOLS });
    case "tools/call": {
      if (typeof params.name !== "string") return error(id, -32602, "tool name required");
      const definition = MCP_TOOLS.find((entry) => entry.name === params.name);
      if (!definition) return error(id, -32602, "unsupported tool");
      const args = params.arguments ?? {};
      if (!isObject(args) || !validArguments(args, definition)) return error(id, -32602, "invalid tool arguments");
      try {
        const call = restCall(definition.name, args);
        const headers = new Headers({ "content-type": "application/json" });
        headers.set("authorization", request.headers.get("authorization") ?? "");
        const response = await dispatch(new Request(new URL(call.path, request.url), {
          method: call.method, headers, ...(call.body ? { body: JSON.stringify(call.body) } : {}),
        }));
        // Do not echo validation strings that may contain caller secrets or internals.
        if (!response.ok) return reply(id, { content: [{ type: "text", text: JSON.stringify({ error: "cloud tool request rejected", status: response.status }) }], isError: true });
        const result: unknown = await response.json();
        return reply(id, { content: [{ type: "text", text: JSON.stringify(result) }], isError: false });
      } catch { return error(id, -32603, "internal error"); }
    }
    default: return error(id, -32601, "method not found");
  }
}
