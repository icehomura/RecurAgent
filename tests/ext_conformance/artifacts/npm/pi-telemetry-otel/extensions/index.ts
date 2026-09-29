import type { ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { type Attributes, type Span, SpanStatusCode, type Tracer, context, trace } from "@opentelemetry/api";
import { registerTelemetryRuntime, unregisterTelemetryRuntime } from "./runtime-registry";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { BasicTracerProvider, BatchSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { SemanticResourceAttributes } from "@opentelemetry/semantic-conventions";
import { clearActiveSpanContext, setActiveSpanContext } from "./span-context-registry";
import { getCurrentAgentName } from "../lib/agent-chain";
import {
  TraceEnvStack,
  isValidTraceId,
  readParentSpanContext,
  readParentSpanContextFromEntries,
  readServiceNameFromEntries,
} from "../lib/trace-env";

function invariant(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

const DEFAULT_ENDPOINT = "http://localhost:4318/v1/traces";
const DEFAULT_SERVICE_NAME = "pi-agent";
const PAYLOAD_MAX_BYTES = 8 * 1024;
const INPUT_PREVIEW_MAX_CHARS = 50;
const TOOL_COMMAND_PREVIEW_MAX_CHARS = 120;
const INPUT_LAST_SENTENCE_MAX_CHARS = 70;
const REDACTION_PATTERN = /api_key|token|secret|password/i;
const DEFAULT_JAEGER_TRACE_URL = "http://localhost:16686/trace";

interface TelemetryConfig {
  endpoint: string;
  headers: Record<string, string>;
  serviceName: string;
  resourceAttributes: Record<string, string>;
}

export interface TelemetryRuntime {
  tracer: Tracer;
  shutdown: () => Promise<void>;
}

export interface TelemetryOptions {
  runtime?: TelemetryRuntime;
  now?: () => number;
}

interface SpanRegistry {
  session?: Span;
  agent?: Span;
  turns: Map<number, Span>;
  tools: Map<string, Span>;
}

interface InputSummary {
  preview: string;
  previewBytes: number;
  previewTruncated: boolean;
  firstSentence?: string;
  firstSentenceTruncated?: boolean;
  lastSentence?: string;
  lastSentenceTruncated?: boolean;
}

function getEnvValue(key: string): string | undefined {
  return process.env[`PI_${key}`] ?? process.env[key];
}

function parseKeyValuePairs(raw: string | undefined): Record<string, string> {
  if (!raw) return {};
  return raw
    .split(",")
    .map((pair) => pair.trim())
    .filter(Boolean)
    .reduce<Record<string, string>>((acc, pair) => {
      const [key, ...rest] = pair.split("=");
      if (!key || rest.length === 0) return acc;
      acc[key.trim()] = rest.join("=").trim();
      return acc;
    }, {});
}

export function resolveServiceName(): string {
  const explicitServiceName = getEnvValue("OTEL_SERVICE_NAME");
  if (explicitServiceName) return explicitServiceName;

  const agentName = getCurrentAgentName();
  if (agentName) return agentName;

  return DEFAULT_SERVICE_NAME;
}

function loadConfig(): TelemetryConfig {
  const endpoint = getEnvValue("OTEL_EXPORTER_OTLP_ENDPOINT") ?? DEFAULT_ENDPOINT;
  const headers = parseKeyValuePairs(getEnvValue("OTEL_EXPORTER_OTLP_HEADERS"));
  const serviceName = resolveServiceName();
  const resourceAttributes = parseKeyValuePairs(getEnvValue("OTEL_RESOURCE_ATTRIBUTES"));

  return {
    endpoint,
    headers,
    serviceName,
    resourceAttributes,
  };
}

function buildJaegerTraceUrl(traceId: string): string {
  return `${DEFAULT_JAEGER_TRACE_URL}/${traceId}`;
}

function getOpenUrlCommand(url: string): { command: string; args: string[] } {
  switch (process.platform) {
    case "darwin":
      return { command: "open", args: [url] };
    case "win32":
      return { command: "cmd", args: ["/c", "start", "", url] };
    default:
      return { command: "xdg-open", args: [url] };
  }
}

async function getTailscaleIp(pi: ExtensionAPI): Promise<string | undefined> {
  try {
    const { stdout, code } = await pi.exec("tailscale", ["status", "--json"], {});
    if (code !== 0) return undefined;

    const data = JSON.parse(stdout) as { Self?: { TailscaleIPs?: unknown[] } };
    const ips = data.Self?.TailscaleIPs ?? [];
    const first = ips.find((ip) => typeof ip === "string");
    return typeof first === "string" ? first : undefined;
  } catch {
    return undefined;
  }
}

async function buildTraceUrls(pi: ExtensionAPI, traceId: string): Promise<{ primary: string; tailscale?: string }> {
  const primary = buildJaegerTraceUrl(traceId);
  const tailscaleIp = await getTailscaleIp(pi);

  if (!tailscaleIp) {
    return { primary };
  }

  return {
    primary,
    tailscale: `http://${tailscaleIp}:16686/trace/${traceId}`,
  };
}

function redactPayload(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => redactPayload(item));
  }

  if (value && typeof value === "object") {
    const output: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(value as Record<string, unknown>)) {
      if (REDACTION_PATTERN.test(key)) {
        output[key] = "[redacted]";
      } else {
        output[key] = redactPayload(entry);
      }
    }
    return output;
  }

  return value;
}

function serializePayload(value: unknown): { text: string; bytes: number; truncated: boolean } {
  const safe = redactPayload(value);
  const text = JSON.stringify(safe) ?? "null";
  const bytes = Buffer.byteLength(text, "utf8");
  if (bytes <= PAYLOAD_MAX_BYTES) {
    return { text, bytes, truncated: false };
  }

  const truncatedText = text.slice(0, PAYLOAD_MAX_BYTES);
  return { text: truncatedText, bytes, truncated: true };
}

function buildPayloadAttributes(prefix: string, value: unknown): Attributes {
  const payload = serializePayload(value);
  return {
    [`${prefix}`]: payload.text,
    [`${prefix}.bytes`]: payload.bytes,
    [`${prefix}.truncated`]: payload.truncated,
  };
}

function normalizeText(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function trimTextByWord(value: string, maxChars: number): { text: string; truncated: boolean } {
  if (value.length <= maxChars) {
    return { text: value, truncated: false };
  }

  const words = value.split(" ");
  let result = "";
  for (const word of words) {
    const next = result ? `${result} ${word}` : word;
    if (next.length > maxChars) break;
    result = next;
  }

  if (!result) {
    return { text: value.slice(0, maxChars), truncated: true };
  }

  return { text: result, truncated: true };
}

function buildInputSummary(text: string): InputSummary {
  const normalized = normalizeText(text);
  const preview = trimTextByWord(normalized, INPUT_PREVIEW_MAX_CHARS);
  const previewBytes = Buffer.byteLength(preview.text, "utf8");

  if (!normalized) {
    return {
      preview: preview.text,
      previewBytes,
      previewTruncated: preview.truncated,
    };
  }

  const sentences = normalized.split(/(?<=[.!?])\s+/).filter(Boolean);
  const firstSentence = trimTextByWord(sentences[0] ?? normalized, INPUT_PREVIEW_MAX_CHARS);
  const lastSentence = trimTextByWord(sentences[sentences.length - 1] ?? normalized, INPUT_LAST_SENTENCE_MAX_CHARS);

  return {
    preview: preview.text,
    previewBytes,
    previewTruncated: preview.truncated,
    firstSentence: firstSentence.text,
    firstSentenceTruncated: firstSentence.truncated,
    lastSentence: lastSentence.text,
    lastSentenceTruncated: lastSentence.truncated,
  };
}

function buildInputSummaryAttributes(prefix: string, summary: InputSummary): Attributes {
  return {
    [`${prefix}.preview`]: summary.preview,
    [`${prefix}.preview.bytes`]: summary.previewBytes,
    [`${prefix}.preview.truncated`]: summary.previewTruncated,
    [`${prefix}.first_sentence`]: summary.firstSentence ?? "",
    [`${prefix}.first_sentence.truncated`]: summary.firstSentenceTruncated ?? false,
    [`${prefix}.last_sentence`]: summary.lastSentence ?? "",
    [`${prefix}.last_sentence.truncated`]: summary.lastSentenceTruncated ?? false,
  };
}

function buildToolSpanName(toolName: string, input: unknown): string {
  if (toolName !== "bash") {
    return `pi.tool: ${toolName}`;
  }

  const command = (input as { command?: unknown } | undefined)?.command;
  if (typeof command !== "string") {
    return `pi.tool: ${toolName}`;
  }

  const normalized = normalizeText(command);
  if (!normalized) {
    return `pi.tool: ${toolName}`;
  }

  const trimmed = trimTextByWord(normalized, TOOL_COMMAND_PREVIEW_MAX_CHARS);
  const suffix = trimmed.truncated ? `${trimmed.text}…` : trimmed.text;
  return `pi.tool: ${toolName}(${suffix})`;
}

function updateSpanName(span: Span | undefined, name: string | undefined): void {
  if (!span || !name) return;
  span.updateName(name);
}

function addSpanEvent(span: Span | undefined, type: string, attrs: Attributes): void {
  if (!span) return;
  span.addEvent(type, {
    "ra.event.type": type,
    ...attrs,
  });
}

function getSessionId(ctx: ExtensionContext): string | undefined {
  return "getSessionId" in ctx.sessionManager ? ctx.sessionManager.getSessionId() : undefined;
}

function findLastStopReason(messages: Array<{ role?: string; stopReason?: string }>): string | undefined {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role === "assistant") {
      return message.stopReason;
    }
  }
  return undefined;
}

function ensureAttribute(span: Span, key: string, value: string | number | boolean | undefined): void {
  if (value === undefined) return;
  span.setAttribute(key, value);
}

function setSpanAttributes(span: Span | undefined, attrs: Attributes): void {
  if (!span) return;
  for (const [key, value] of Object.entries(attrs)) {
    span.setAttribute(key, value as string | number | boolean);
  }
}

function createTracer(config: TelemetryConfig): TelemetryRuntime {
  const resource = resourceFromAttributes({
    ...config.resourceAttributes,
    [SemanticResourceAttributes.SERVICE_NAME]: config.serviceName,
  });

  const exporter = new OTLPTraceExporter({ url: config.endpoint, headers: config.headers });
  const processor = new BatchSpanProcessor(exporter);
  const provider = new BasicTracerProvider({
    resource,
    spanProcessors: [processor],
  });

  return {
    tracer: provider.getTracer("pi-telemetry-otel", "0.1.0"),
    shutdown: async () => {
      await provider.forceFlush();
      await provider.shutdown();
    },
  };
}

export default function telemetryOtelExtension(pi: ExtensionAPI, options: TelemetryOptions = {}): void {
  const config = loadConfig();
  const now = options.now ?? Date.now;

  // Tracer creation is deferred until ensureSessionSpan() so we can read a
  // service-name override from session entries (injected by parent sessions
  // for in-process sub-agents like the ask tool).
  let runtime: TelemetryRuntime | undefined = options.runtime;
  let tracer: Tracer | undefined = options.runtime?.tracer;
  let shutdown: (() => Promise<void>) | undefined = options.runtime?.shutdown;

  function ensureRuntime(serviceNameOverride?: string): TelemetryRuntime {
    if (runtime) return runtime;
    const effectiveConfig = serviceNameOverride
      ? { ...config, serviceName: serviceNameOverride }
      : config;
    runtime = createTracer(effectiveConfig);
    tracer = runtime.tracer;
    shutdown = runtime.shutdown;
    return runtime;
  }

  const spans: SpanRegistry = {
    turns: new Map(),
    tools: new Map(),
  };

  const traceEnvStack = new TraceEnvStack<Span>();
  const traceScopeKeys = {
    session: "session",
    agent: "agent",
    turn: (turnIndex: number) => `turn:${turnIndex}`,
    tool: (toolCallId: string) => `tool:${toolCallId}`,
  };

  let sessionId: string | undefined;
  let activeTurnIndex: number | undefined;
  let lastInputSummary: InputSummary | undefined;
  let shutdownTriggered = false;

  function updateTraceStatus(ctx: ExtensionContext, traceId: string | undefined): void {
    if (!ctx.hasUI) return;
    if (!traceId) {
      ctx.ui.setStatus("telemetry-otel", undefined);
      return;
    }
    const theme = ctx.ui.theme;
    const shortTraceId = traceId.slice(0, 8);
    ctx.ui.setStatus("telemetry-otel", theme.fg("dim", `trace ${shortTraceId}`));
  }

  function syncActiveSpanContext(): void {
    if (!sessionId) return;
    const top = traceEnvStack.peek();
    if (!top) {
      clearActiveSpanContext(sessionId);
      return;
    }

    setActiveSpanContext(sessionId, {
      ...top.spanContext(),
      isRemote: false,
    });
  }

  function ensureSessionSpan(ctx: ExtensionContext): Span {
    if (spans.session) return spans.session;

    sessionId = getSessionId(ctx);
    const entries = ctx.sessionManager.getEntries();

    // Prefer trace context from session entries (injected by parent session for
    // in-process sub-sessions) over env vars (which race across concurrent subs).
    // Fall back to env vars for cross-process propagation (RPC sub-agents).
    const parentSpanContext =
      readParentSpanContextFromEntries(entries) ?? readParentSpanContext();
    const parentContext = parentSpanContext
      ? trace.setSpanContext(context.active(), parentSpanContext)
      : context.active();

    // Initialize runtime with service name override from entries (if present).
    // This allows in-process sub-sessions to appear with a distinct service
    // name in the trace (e.g. "pi-ask-agent" instead of "pi-agent").
    const serviceNameOverride = readServiceNameFromEntries(entries);
    const ensured = ensureRuntime(serviceNameOverride);
    if (sessionId) {
      registerTelemetryRuntime(sessionId, ensured);
    }

    invariant(tracer, "Tracer not initialized");
    const span = tracer.startSpan(
      "ra.session",
      {
        startTime: now(),
      },
      parentContext,
    );

    ensureAttribute(span, "ra.session.id", sessionId);
    ensureAttribute(span, "ra.session.file", ctx.sessionManager.getSessionFile());

    traceEnvStack.push(traceScopeKeys.session, span);
    syncActiveSpanContext();
    updateTraceStatus(ctx, span.spanContext().traceId);
    spans.session = span;
    return span;
  }

  function endSpan(span: Span | undefined, endTime?: number): void {
    if (!span) return;
    span.end(endTime);
  }

  function closeOrphanToolSpans(reason: string): void {
    for (const [toolCallId, span] of spans.tools.entries()) {
      span.setStatus({ code: SpanStatusCode.ERROR, message: `Closed without tool_result (${reason})` });
      span.setAttribute("ra.tool.missing_result", true);
      span.end();
      traceEnvStack.pop(traceScopeKeys.tool(toolCallId));
    }
    spans.tools.clear();
    syncActiveSpanContext();
  }

  function closeOrphanTurnSpans(reason: string): void {
    for (const [turnIndex, span] of spans.turns.entries()) {
      span.setStatus({ code: SpanStatusCode.ERROR, message: `Closed without turn_end (${reason})` });
      span.setAttribute("ra.turn.missing_end", true);
      span.end();
      traceEnvStack.pop(traceScopeKeys.turn(turnIndex));
    }
    spans.turns.clear();
    activeTurnIndex = undefined;
    syncActiveSpanContext();
  }

  function closeOrphanAgentSpan(reason: string): void {
    if (!spans.agent) return;
    spans.agent.setStatus({ code: SpanStatusCode.ERROR, message: `Closed without agent_end (${reason})` });
    spans.agent.setAttribute("ra.agent.missing_end", true);
    spans.agent.end();
    spans.agent = undefined;
    traceEnvStack.pop(traceScopeKeys.agent);
    syncActiveSpanContext();
  }

  function sendTraceMessage(ctx: ExtensionCommandContext, message: string, type: "info" | "error" = "info"): void {
    if (ctx.hasUI) {
      ctx.ui.notify(message, type);
      return;
    }

    pi.sendMessage({
      customType: "telemetry-otel-trace-url",
      content: message,
      display: true,
    });
  }

  async function openUrl(ctx: ExtensionCommandContext, url: string): Promise<void> {
    const { command, args } = getOpenUrlCommand(url);
    const { code, stderr } = await pi.exec(command, args, {});
    if (code !== 0) {
      const error = stderr.trim() || `Failed to open URL (exit ${code}).`;
      sendTraceMessage(ctx, error, "error");
    }
  }

  pi.registerCommand("open-jaeger-trace", {
    description: "Open the current Jaeger trace in your browser",
    handler: async (_args, ctx) => {
      const sessionSpan = ensureSessionSpan(ctx);
      const traceId = sessionSpan.spanContext().traceId;
      if (!traceId || !isValidTraceId(traceId)) {
        sendTraceMessage(ctx, "No valid trace ID found for this session.", "error");
        return;
      }

      const urls = await buildTraceUrls(pi, traceId);
      const messageLines = [`Trace URL: ${urls.primary}`];
      if (urls.tailscale) {
        messageLines.push(`Tailscale URL: ${urls.tailscale}`);
      }
      const message = messageLines.join("\n");

      if (!ctx.hasUI) {
        sendTraceMessage(ctx, message, "info");
        return;
      }

      const confirm = await ctx.ui.confirm("Open Jaeger trace?", `${message}\n\nOpen in your browser?`);
      if (!confirm) {
        sendTraceMessage(ctx, message, "info");
        return;
      }

      await openUrl(ctx, urls.primary);
      sendTraceMessage(ctx, message, "info");
    },
  });

  pi.on("session_start", (_event, ctx) => {
    sessionId = getSessionId(ctx);
    lastInputSummary = undefined;
    ensureSessionSpan(ctx);
  });

  pi.on("session_switch", (_event, ctx) => {
    closeOrphanToolSpans("session_switch");
    closeOrphanTurnSpans("session_switch");
    closeOrphanAgentSpan("session_switch");

    endSpan(spans.session, now());
    spans.session = undefined;
    traceEnvStack.clear();
    if (sessionId) {
      clearActiveSpanContext(sessionId);
    }
    updateTraceStatus(ctx, undefined);
    sessionId = getSessionId(ctx);
    lastInputSummary = undefined;
    ensureSessionSpan(ctx);
  });

  pi.on("input", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const payloadAttributes = buildPayloadAttributes("ra.input.text", event.text);
    const summary = buildInputSummary(event.text);

    lastInputSummary = summary;

    const summaryAttributes = buildInputSummaryAttributes("ra.input", summary);

    addSpanEvent(sessionSpan, "input", {
      "ra.event.source": event.source,
      "ra.session.id": sessionId ?? "",
      "ra.input.images": event.images?.length ?? 0,
      ...payloadAttributes,
      ...summaryAttributes,
    });

    setSpanAttributes(sessionSpan, buildInputSummaryAttributes("ra.input.latest", summary));
    updateSpanName(sessionSpan, summary.preview ? `pi.session ${summary.preview}` : undefined);
  });

  pi.on("agent_start", (_event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const parent = trace.setSpan(context.active(), sessionSpan);
    invariant(tracer, "Tracer not initialized");
    const span = tracer.startSpan("ra.agent", { startTime: now() }, parent);

    ensureAttribute(span, "ra.session.id", sessionId);

    if (lastInputSummary) {
      setSpanAttributes(span, buildInputSummaryAttributes("ra.input.latest", lastInputSummary));
      updateSpanName(span, lastInputSummary.preview ? `pi.agent ${lastInputSummary.preview}` : undefined);
    }

    traceEnvStack.push(traceScopeKeys.agent, span);
    syncActiveSpanContext();
    updateTraceStatus(ctx, span.spanContext().traceId);
    spans.agent = span;
  });

  pi.on("turn_start", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const parent = trace.setSpan(context.active(), spans.agent ?? sessionSpan);

    activeTurnIndex = event.turnIndex;

    invariant(tracer, "Tracer not initialized");
    const span = tracer.startSpan(
      "ra.turn",
      {
        startTime: event.timestamp ?? now(),
      },
      parent,
    );

    ensureAttribute(span, "ra.session.id", sessionId);
    ensureAttribute(span, "ra.turn.index", event.turnIndex);

    if (lastInputSummary) {
      setSpanAttributes(span, buildInputSummaryAttributes("ra.input.latest", lastInputSummary));
    }

    spans.turns.set(event.turnIndex, span);
    traceEnvStack.push(traceScopeKeys.turn(event.turnIndex), span);
    syncActiveSpanContext();
    addSpanEvent(span, "turn_start", {
      "ra.turn.index": event.turnIndex,
      "ra.session.id": sessionId ?? "",
    });
  });

  pi.on("tool_call", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const parentSpan = activeTurnIndex !== undefined ? spans.turns.get(activeTurnIndex) : spans.agent;
    const parent = trace.setSpan(context.active(), parentSpan ?? sessionSpan);

    invariant(tracer, "Tracer not initialized");
    const span = tracer.startSpan(buildToolSpanName(event.toolName, event.input), { startTime: now() }, parent);

    ensureAttribute(span, "ra.session.id", sessionId);
    ensureAttribute(span, "ra.turn.index", activeTurnIndex);
    ensureAttribute(span, "ra.tool.name", event.toolName);
    ensureAttribute(span, "ra.tool.call_id", event.toolCallId);

    spans.tools.set(event.toolCallId, span);
    traceEnvStack.push(traceScopeKeys.tool(event.toolCallId), span);
    syncActiveSpanContext();

    addSpanEvent(span, "tool_call", {
      "ra.tool.name": event.toolName,
      "ra.tool.call_id": event.toolCallId,
      "ra.session.id": sessionId ?? "",
      "ra.turn.index": activeTurnIndex ?? -1,
      ...buildPayloadAttributes("ra.tool.input", event.input),
    });
  });

  pi.on("tool_result", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const span = spans.tools.get(event.toolCallId);

    const outputPayload = {
      content: event.content,
      details: event.details,
    };

    addSpanEvent(span ?? sessionSpan, "tool_result", {
      "ra.tool.name": event.toolName,
      "ra.tool.call_id": event.toolCallId,
      "ra.tool.is_error": event.isError,
      "ra.session.id": sessionId ?? "",
      "ra.turn.index": activeTurnIndex ?? -1,
      ...buildPayloadAttributes("ra.tool.output", outputPayload),
    });

    if (span) {
      if (event.isError) {
        span.setStatus({ code: SpanStatusCode.ERROR, message: "tool_result error" });
      }
      span.end();
      spans.tools.delete(event.toolCallId);
    }

    traceEnvStack.pop(traceScopeKeys.tool(event.toolCallId));
    syncActiveSpanContext();
  });

  pi.on("turn_end", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const span = spans.turns.get(event.turnIndex);

    const turnStopReason = "stopReason" in event.message ? event.message.stopReason : undefined;

    addSpanEvent(span ?? sessionSpan, "turn_end", {
      "ra.turn.index": event.turnIndex,
      "ra.session.id": sessionId ?? "",
      "ra.turn.tool_results": event.toolResults.length,
      "ra.message.stop_reason": turnStopReason ?? "",
    });

    endSpan(span, now());
    spans.turns.delete(event.turnIndex);
    traceEnvStack.pop(traceScopeKeys.turn(event.turnIndex));
    syncActiveSpanContext();
    if (activeTurnIndex === event.turnIndex) {
      activeTurnIndex = undefined;
    }
  });

  pi.on("agent_end", async (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    const stopReason = findLastStopReason(event.messages);

    addSpanEvent(spans.agent ?? sessionSpan, "agent_end", {
      "ra.session.id": sessionId ?? "",
      "ra.message.stop_reason": stopReason ?? "",
    });

    closeOrphanToolSpans("agent_end");
    closeOrphanTurnSpans("agent_end");

    endSpan(spans.agent, now());
    spans.agent = undefined;
    traceEnvStack.pop(traceScopeKeys.agent);
    syncActiveSpanContext();

    if (!ctx.hasUI && !shutdownTriggered) {
      shutdownTriggered = true;
      endSpan(spans.session, now());
      spans.session = undefined;
      traceEnvStack.clear();
      if (sessionId) {
        clearActiveSpanContext(sessionId);
        unregisterTelemetryRuntime(sessionId);
      }
      await shutdown?.();
    }
  });

  pi.on("model_select", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    addSpanEvent(sessionSpan, "model_select", {
      "ra.session.id": sessionId ?? "",
      "ra.model.provider": event.model.provider,
      "ra.model.id": event.model.id,
      "ra.model.source": event.source,
    });
  });

  pi.on("session_compact", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    addSpanEvent(sessionSpan, "session_compact", {
      "ra.session.id": sessionId ?? "",
      "ra.compaction.first_kept_entry": event.compactionEntry?.firstKeptEntryId ?? "",
      "ra.compaction.tokens_before": event.compactionEntry?.tokensBefore ?? 0,
      ...buildPayloadAttributes("ra.compaction.summary", event.compactionEntry?.summary),
    });
  });

  pi.on("session_tree", (event, ctx) => {
    const sessionSpan = ensureSessionSpan(ctx);
    addSpanEvent(sessionSpan, "session_tree", {
      "ra.session.id": sessionId ?? "",
      "ra.tree.new_leaf": event.newLeafId ?? "",
      "ra.tree.old_leaf": event.oldLeafId ?? "",
      "ra.tree.from_extension": event.fromExtension ?? false,
    });
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    if (shutdownTriggered) return;
    shutdownTriggered = true;

    ensureSessionSpan(ctx);
    closeOrphanToolSpans("session_shutdown");
    closeOrphanTurnSpans("session_shutdown");
    closeOrphanAgentSpan("session_shutdown");

    endSpan(spans.session, now());
    spans.session = undefined;
    traceEnvStack.clear();
    updateTraceStatus(ctx, undefined);

    if (sessionId) {
      clearActiveSpanContext(sessionId);
      unregisterTelemetryRuntime(sessionId);
    }

    await shutdown?.();
  });
}
