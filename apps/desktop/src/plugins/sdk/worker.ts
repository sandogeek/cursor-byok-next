import * as host from "cursor-byok-next:host";
declare function __entry(): string;
declare function __send(frame: string): void;
import {
  ProtocolHttpError,
  __getRegisteredPlugin,
  type JsonValue,
  type NetworkEventStream,
  type PluginContext,
} from "cursor-byok-next:plugin";
import type { ModelEvent, ProviderSupport } from "cursor-byok-next:provider";
import type { ResourceAddMethod, ResourceSupport } from "cursor-byok-next:resource";

await import(__entry());
const plugin = __getRegisteredPlugin();
const controllers = new Map<string, AbortController>();
function send(value: unknown): Promise<void> { __send(JSON.stringify(value) + "\n"); return Promise.resolve(); }
async function hostCall(requestId: string, method: string, params: unknown): Promise<unknown> {
 try {
  const p = (params ?? {}) as Record<string, unknown>;
  switch (method) {
    case "network.fetch": return await host.fetch(requestId, String(p.url), JSON.stringify(p)).then(JSON.parse);
    case "network.stream.open": return await host.streamOpen(requestId, String(p.url), JSON.stringify(p)).then(JSON.parse);
    case "network.stream.read": return await host.streamRead(requestId, String(p.streamId)).then(JSON.parse);
    case "network.stream.close": return await host.streamClose(requestId, String(p.streamId));
    case "protocol.stream": return await host.protocolStream(requestId, JSON.stringify(p)).then(JSON.parse);
    default: throw new Error("unsupported host method");
  }
 } catch (error) {
   if (error && typeof error === "object" && "status" in error && typeof error.status === "number") {
     throw new ProtocolHttpError(error.status, "upstream request failed");
   }
   throw error;
 }
}

async function* streamLines(requestId: string, streamId: string): AsyncGenerator<string> {
  try {
    for (;;) {
      const chunk = await hostCall(requestId, "network.stream.read", { streamId }) as {
        lines: string[];
        done: boolean;
      };
      for (const line of chunk.lines) yield line;
      if (chunk.done) return;
    }
  } finally {
    void hostCall(requestId, "network.stream.close", { streamId }).catch(() => undefined);
  }
}

function contextFor(requestId: string, signal: AbortSignal): PluginContext {
  return {
    network: {
      fetch: (url, init = {}) => hostCall(requestId, "network.fetch", { url, ...init }) as ReturnType<PluginContext["network"]["fetch"]>,
      stream: async (url, init = {}): Promise<NetworkEventStream> => {
        const opened = await hostCall(requestId, "network.stream.open", { url, ...init }) as {
          streamId: string;
          status: number;
          headers: Record<string, string>;
        };
        return {
          status: opened.status,
          headers: opened.headers,
          lines: streamLines(requestId, opened.streamId),
        };
      },
    },
    protocol: {
      stream: (input) => hostCall(requestId, "protocol.stream", input).then(() => undefined),
    },
    signal,
  };
}

function provider(id: unknown): ProviderSupport {
  const found = plugin.providers.find((item) => item.id === id);
  if (!found) throw new Error(`unknown plugin provider: ${id}`);
  return found;
}

function resourceSupport(type: unknown): ResourceSupport {
  const found = (plugin.resources ?? []).find((resource) => resource.type === type);
  if (!found) throw new Error(`unknown plugin resource type: ${type}`);
  return found;
}

function addMethod(support: ResourceSupport, methodId: unknown): ResourceAddMethod {
  const found = (support.add ?? []).find((method) => method.id === methodId);
  if (!found) throw new Error(`unknown plugin add method: ${methodId}`);
  return found;
}

async function dispatch(message: { id: string; method: string; params?: JsonValue }) {
  const controller = new AbortController();
  controllers.set(message.id, controller);
  const context = contextFor(message.id, controller.signal);
  const params = (message.params ?? {}) as Record<string, JsonValue>;
  try {
    let result: unknown;
    switch (message.method) {
      case "provider.invoke": {
        const output = {
          emit: (event: ModelEvent) => void send({ type: "event", id: message.id, event }),
        };
        let request: Promise<unknown> | undefined;
        result = await provider(params.providerId).invoke(
          {
            model: params.model as never,
            resource: (params.resource ?? null) as never,
            readRequest: () => {
              request ??= (async () => JSON.parse(host.readRequest(message.id)))();
              return request as never;
            },
          },
          output,
          context,
        );
        break;
      }
      case "models.list": {
        const models = provider(params.providerId).models;
        if (!models) throw new Error(`plugin provider ${params.providerId} has no models`);
        result = await models.list({ resource: (params.resource ?? null) as never }, context);
        break;
      }
      case "resource.present": {
        const support = resourceSupport(params.resourceType);
        const resources = Array.isArray(params.resources) ? params.resources : [];
        result = resources.map((resource) => support.present(resource as never));
        break;
      }
      case "resource.refresh": {
        const support = resourceSupport(params.resourceType);
        if (!support.refresh) throw new Error(`resource ${params.resourceType} has no refresh`);
        result = await support.refresh(params.resource as never, context);
        break;
      }
      case "resource.remove": {
        const support = resourceSupport(params.resourceType);
        await support.remove?.(params.resource as never, context);
        result = null;
        break;
      }
      case "oauth.begin": {
        const support = resourceSupport(params.resourceType);
        const method = addMethod(support, params.methodId);
        result = method.type === "oauth2.authorization-code" ? await method.begin(params.authorization as never, context) : await method.begin(context);
        break;
      }
      case "oauth.poll": {
        const support = resourceSupport(params.resourceType);
        const method = addMethod(support, params.methodId);
        if (method.type !== "oauth2.0") throw new Error(`add method ${method.id} does not support polling`);
        result = await method.poll(params.session ?? null, context);
        break;
      }
      case "oauth.complete": {
        const support = resourceSupport(params.resourceType);
        const method = addMethod(support, params.methodId);
        if (method.type !== "oauth2.authorization-code") {
          throw new Error(`add method ${method.id} does not support authorization-code completion`);
        }
        result = await method.complete(
          params.session ?? null,
          params.authorization as never,
          context,
        );
        break;
      }
      case "form.submit": {
        const support = resourceSupport(params.resourceType);
        const method = addMethod(support, params.methodId);
        if (method.type !== "form") throw new Error(`add method ${method.id} does not support form submit`);
        const values = params.values && typeof params.values === "object" && !Array.isArray(params.values)
          ? Object.fromEntries(Object.entries(params.values as Record<string, JsonValue>).map(([key, value]) => [key, String(value ?? "")]))
          : {};
        result = await method.submit(values, context);
        break;
      }
      default:
        throw new Error(`unknown plugin method: ${message.method}`);
    }
    await send({ type: "result", id: message.id, result: result ?? null });
  } catch (error) {
    await send({ type: "result", id: message.id, error: error instanceof Error ? error.message : String(error) });
  } finally {
    controllers.delete(message.id);
  }
}

// Rust calls synchronously; invocation work settles through the VM microtask queue.
(globalThis as unknown as { __recv: (line: string) => void }).__recv = (line: string) => {
  const message = JSON.parse(line);
  if (message.type === "request") void dispatch(message);
  else if (message.type === "cancel") controllers.get(message.id)?.abort();
};
