import type { ProviderSupport } from "./provider.ts";
import type { ResourceSupport } from "./resource.ts";

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

/**
 * 可本地化文本:纯字符串,或 locale → 文本 的映射
 * (如 { "zh-CN": "账号", "en-US": "Accounts" })。
 * 宿主原样透传,由界面按当前语言解析;模型名等来自上游的数据保持纯字符串。
 */
export type LocalizedText = string | { [locale: string]: string };

export type NetworkRequestInit = {
  method?: string;
  headers?: Record<string, string>;
  body?: string;
};

export type NetworkResponse = {
  status: number;
  headers: Record<string, string>;
  body: string;
};

/** 流式响应体,按行随到随交付(用于 SSE)。 */
export type NetworkEventStream = {
  status: number;
  headers: Record<string, string>;
  lines: AsyncIterable<string>;
};

/**
 * 每次能力调用收到的宿主服务。网络请求默认仅限 plugin.json 声明的 HTTPS 主机;
 * `permissions.resourceOrigin` 为 true 时,本次调用还可访问绑定资源的 origin(可 HTTP)。
 * 宿主取消本次调用时通过 `signal` 中止。
 */
export type PluginContext = {
  network: {
    fetch(url: string, init?: NetworkRequestInit): Promise<NetworkResponse>;
    stream(url: string, init?: NetworkRequestInit): Promise<NetworkEventStream>;
  };
  protocol: {
    /** 宿主构建请求体、发起 HTTP、解析 SSE；事件写入本次 invocation 流。 */
    stream(input: ProtocolStreamInput): Promise<void>;
  };
  signal: AbortSignal;
};

export type ProtocolStreamInput = {
  protocol: "openai-chat" | "openai-responses" | "anthropic";
  /** 上游固定端点；origin 必须是本次绑定资源或 plugin.json 声明的 HTTPS 主机。 */
  url: string;
  /** 发给网关的裸上游模型 ID。 */
  model: string;
  headers?: Record<string, string>;
};

/** 上游返回非 2xx 时抛出，携带截断后的响应体供调用方分类。 */
export class ProtocolHttpError extends Error {
  constructor(readonly status: number, readonly body: string) {
    super(`HTTP ${status}: ${body}`);
  }
}

/**
 * Provider 插件定义:一组能力实现的集合。插件不持有任何持久状态——
 * 资源与模型目录由宿主存储,每次调用所需的数据都通过参数传入。
 */
export type ProviderPluginDefinition = {
  providers: ProviderSupport[];
  resources?: ResourceSupport[];
};

let registered: ProviderPluginDefinition | undefined;

/** 注册 Provider 插件;每个插件入口只能调用一次。 */
export function defineProviderPlugin(definition: ProviderPluginDefinition): ProviderPluginDefinition {
  if (registered) throw new Error("defineProviderPlugin can only be called once");
  registered = definition;
  return definition;
}

export function __getRegisteredPlugin(): ProviderPluginDefinition {
  if (!registered) throw new Error("plugin entry must call defineProviderPlugin");
  return registered;
}

/** 可序列化的能力摘要,宿主收集它时不调用任何能力方法。 */
export function __descriptor(definition: ProviderPluginDefinition) {
  return {
    providers: definition.providers.map((provider) => ({
      id: provider.id,
      displayName: provider.displayName,
      description: provider.description ?? null,
      providerType: provider.providerType,
      resourceType: provider.resourceType ?? null,
      hasModels: provider.models !== undefined,
      catalog: provider.models?.catalog ?? "snapshot",
    })),
    resources: (definition.resources ?? []).map((resource) => ({
      type: resource.type,
      displayName: resource.displayName,
      add: (resource.add ?? []).map((method) => ({
        type: method.type,
        id: method.id,
        displayName: method.displayName,
        description: method.type === "form" ? null : method.description ?? null,
        callback: method.type === "oauth2.authorization-code"
          ? {
            port: method.callback?.port ?? null,
            path: method.callback?.path ?? "/oauth-callback",
          }
          : null,
        fields: method.type === "form"
          ? method.fields.map((field) => ({
            id: field.id,
            label: field.label,
            type: field.type,
            required: field.required ?? false,
            placeholder: field.placeholder ?? null,
          }))
          : [],
      })),
      canRefresh: resource.refresh !== undefined,
      canRemove: resource.remove !== undefined,
    })),
  };
}
