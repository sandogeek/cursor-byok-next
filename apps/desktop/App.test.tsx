import { expect, open, test } from "nsh/test";
import type { CallDetail, CallRecord, CallsSnapshot, HarnessState, PluginSnapshot } from "cursor-byok-next/native";

const missing: HarnessState = {
  platform: "macos",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
  preferredProxyPort: null,
  dataDir: "/Users/test/.cursor-byok-next",
};
const untrusted: HarnessState = {
  ...missing,
  ca: "untrusted",
  caInstallCommand: "sudo security add-trusted-cert placeholder-ca.crt",
};
const ready: HarnessState = { ...missing, ca: "ready" };
const enabled: HarnessState = {
  ...ready,
  integration: "enabled",
  settingsApplied: true,
  proxyUrl: "http://127.0.0.1:39831",
  proxyPort: 39831,
};

const magpieReady: PluginSnapshot = {
  runtimeReady: true,
  plugins: [{
    id: "dev.cursorbyoknext.plugins.magpie",
    name: "Magpie",
    version: "0.1.0",
    icon: "",
    runtimeReady: true,
    providers: [{
      id: "magpie",
      pluginId: "dev.cursorbyoknext.plugins.magpie",
      displayName: "Magpie",
      description: "从 Magpie 网关实时读取目录。",
      providerType: "magpie",
      resourceType: "gateway",
      catalog: "live",
      configured: true,
      models: [{
        id: "plugin:dev.cursorbyoknext.plugins.magpie/magpie/resource-1/openai/gpt-5",
        pluginId: "dev.cursorbyoknext.plugins.magpie",
        pluginName: "Magpie",
        providerId: "magpie",
        modelId: "resource-1/openai/gpt-5",
        displayName: "GPT-5",
      }],
    }],
    resources: [{
      resourceType: "gateway",
      displayName: "网关",
      add: [{
        id: "connect",
        displayName: "连接 Magpie",
        fields: [
          { id: "base-url", label: "网关地址", fieldType: "url", required: true, placeholder: "http://127.0.0.1:3425" },
          { id: "api-key", label: "API 密钥", fieldType: "secret", required: true, placeholder: "magpie-cursor-byok-next" },
        ],
      }],
      resources: [{
        id: "resource-1",
        status: "ready",
        displayName: "http://127.0.0.1:3425",
        description: "",
      }],
    }],
  }],
};

const sampleCall: CallRecord = {
  callId: "call-1",
  runId: "run-1",
  conversationId: "conversation-1",
  providerCallIndex: 0,
  modelHash: "plugin:dev.cursorbyoknext.plugins.magpie/magpie/resource-1/openai/gpt-5",
  providerType: "plugin",
  providerUrl: "plugin://dev.cursorbyoknext.plugins.magpie/magpie",
  requestType: "plugin",
  requestUrl: "plugin://dev.cursorbyoknext.plugins.magpie/magpie",
  modelId: "openai/gpt-5",
  displayName: "GPT-5",
  fast: false,
  status: "completed",
  finishReason: "stop",
  createdAtMs: 1_700_000_000_000,
  durationMs: 42,
  inputTokens: 10,
  outputTokens: 4,
  totalTokens: 14,
  messageCount: 1,
  toolCount: 0,
  requestBytes: 12,
  responseBytes: 20,
  responseChunkCount: 1,
  httpStatus: 200,
  detailed: true,
  requestTruncated: false,
  responseTruncated: false,
};

const emptyCalls: CallsSnapshot = {
  calls: [],
  detailed: false,
  storage: { callCount: 0, requestCount: 0, chunkCount: 0 },
  degraded: false,
};

const recordedCalls: CallsSnapshot = {
  calls: [sampleCall],
  detailed: true,
  storage: { callCount: 1, requestCount: 1, chunkCount: 1 },
  degraded: false,
};

const sampleDetail: CallDetail = {
  call: sampleCall,
  request: {
    headersJson: "{\"content-type\":\"application/json\"}",
    bodyJson: "{\"model\":\"openai/gpt-5\"}",
    byteCount: 12,
  },
  chunks: [{ seq: 0, receivedOffsetMs: 12, data: "data: [DONE]\\n\\n", byteCount: 14 }],
};

function options(initial: HarnessState, calls: string[] = [], plugins: PluginSnapshot = { runtimeReady: false, runtimeError: "bun is not on PATH", plugins: [] }, records: CallsSnapshot = emptyCalls) {
  let current = initial;
  let catalog = plugins;
  let log = records;
  return {
    width: 1100,
    height: 720,
    platform: "macos" as const,
    locale: "zh-CN" as const,
    host: {
      handlers: {
        "cursor.status": () => current,
        "cursor.initializeCa": () => {
          calls.push("initializeCa");
          current = untrusted;
          return current;
        },
        "cursor.setEnabled": ({ enabled: next }: { enabled: boolean }) => {
          calls.push(next ? "enable" : "disable");
          current = next ? enabled : ready;
          return current;
        },
        "cursor.setPreferredPort": ({ port }: { port: number | null }) => {
          calls.push(`port:${port}`);
          current = { ...current, preferredProxyPort: port };
          return current;
        },
        "plugins.snapshot": () => catalog,
        "plugins.submitForm": () => {
          calls.push("submitForm");
          return { added: 1, updated: 0 };
        },
        "plugins.deleteResource": ({ resourceId }: { resourceId: string }) => {
          calls.push(`delete:${resourceId}`);
          catalog = {
            ...catalog,
            plugins: catalog.plugins.map((plugin) => ({
              ...plugin,
              resources: plugin.resources.map((resource) => ({ ...resource, resources: [] })),
              providers: plugin.providers.map((provider) => ({ ...provider, configured: false, models: [] })),
            })),
          };
          return catalog;
        },
        "plugins.refreshCatalog": () => {
          calls.push("refreshCatalog");
          return catalog;
        },
        "calls.snapshot": () => log,
        "calls.detail": ({ callId }: { callId: string }) => {
          calls.push(`detail:${callId}`);
          return sampleDetail;
        },
        "calls.setDetailed": ({ enabled: next }: { enabled: boolean }) => {
          calls.push(next ? "detailed-on" : "detailed-off");
          log = { ...log, detailed: next };
          return log;
        },
        "calls.clearDetails": () => {
          calls.push("clear-details");
          log = { ...log, storage: { ...log.storage, requestCount: 0, chunkCount: 0 } };
          return log;
        },
        "calls.clearAll": () => {
          calls.push("clear-all");
          log = emptyCalls;
          return log;
        },
      },
    },
    modules: {
      "nsh": {
        "Clipboard.writeText": async (text: string) => { calls.push(`copy:${text}`); },
      },
    },
  };
}

test("复刻接管壳，CA 未信任时不能开启接管", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(missing, calls));
  expect(app.texts()).toContain("数据概览");
  expect(app.texts()).toContain("调用详细");
  expect(app.texts()).toContain("模型配置");
  expect(app.texts()).toContain("插件配置");
  expect(app.texts()).toContain("需要先初始化本地 CA");
  expect(app.enabled("开启接管 Cursor")).toBe(false);
  await app.press("初始化 CA");
  expect(app.texts()).toContain("安装本地 CA");
  expect(calls).toEqual(["initializeCa"]);
  await app.press("复制命令");
  expect(app.texts()).toContain("命令已复制");
  expect(calls[1]?.startsWith("copy:sudo security")).toBe(true);
});

test("关闭接管先确认，再调用真实状态边界", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(enabled, calls));
  expect(app.texts()).toContain("已接管");
  expect(app.texts()).toContain("Cursor 接管已生效");
  await app.press("关闭接管 Cursor");
  expect(app.texts()).toContain("关闭接管 Cursor？");
  expect(calls).toEqual([]);
  await app.press("取消");
  expect(calls).toEqual([]);
  await app.press("关闭接管 Cursor");
  await app.press("关闭接管");
  expect(calls).toEqual(["disable"]);
  expect(app.texts()).toContain("本地 CA 已就绪");
});

test("代理与设置不一致时显示降级状态", async () => {
  const app = await open("./test-fixtures/App.tsx", options({ ...ready, integration: "degraded", proxyUrl: "http://127.0.0.1:39831" }));
  expect(app.texts()).toContain("状态异常");
  expect(app.texts()).toContain("本地代理与 Cursor 配置不一致，请刷新状态或重新开启接管。");
});

test("插件页显示 Magpie 目录且不含密钥", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(ready, calls, magpieReady));
  await app.press("插件配置");
  expect(app.texts()).toContain("Magpie");
  expect(app.texts()).toContain("GPT-5");
  expect(app.texts()).toContain("plugin:dev.cursorbyoknext.plugins.magpie/magpie/resource-1/openai/gpt-5");
  expect(app.texts()).toContain("1 个模型 · 仅本页可见");
  expect(app.texts().join("\n")).not.toContain("sk-");
  expect(app.texts().join("\n")).not.toContain("privateData");
  await app.press("刷新目录");
  expect(calls).toEqual(["refreshCatalog"]);
});

test("已安装 Bun 时，插件加载失败可重试而不会误报缺少 Bun", async () => {
  const setup = options(ready, [], { runtimeReady: true, runtimeError: "内置插件加载失败", plugins: [] });
  let loads = 0;
  setup.host.handlers["plugins.snapshot"] = () => ++loads === 1
    ? { runtimeReady: true, runtimeError: "内置插件加载失败", plugins: [] }
    : magpieReady;
  const app = await open("./test-fixtures/App.tsx", setup);
  await app.press("插件配置");
  expect(app.texts()).toContain("无法加载 Magpie 插件");
  expect(app.texts()).not.toContain("需要 Bun 才能加载 Magpie 插件");
  await app.press("刷新状态");
  expect(app.texts()).toContain("GPT-5");
});

test("网关数据读取失败时不显示首次配置的空状态", async () => {
  const plugin = magpieReady.plugins[0]!;
  const gatewayType = plugin.resources[0]!;
  const app = await open("./test-fixtures/App.tsx", options(ready, [], {
    ...magpieReady,
    plugins: [{ ...plugin, resources: [{ ...gatewayType, resources: [], storageError: "无法读取已保存的网关" }] }],
  }));
  await app.press("插件配置");
  expect(app.texts()).toContain("无法读取已保存的网关");
  expect(app.texts()).not.toContain("还没有 Magpie 网关。填写地址和 API 密钥后保存。");
  expect(app.enabled("保存连接")).toBe(false);
});

test("界面语言跟随设备，切换后立即翻译并记住选择", async () => {
  const app = await open("./test-fixtures/App.tsx", options(missing));
  expect(app.texts()).toContain("数据概览");
  await app.press("系统设置");
  await app.press("id:settings-language");
  await app.press("English");
  expect(app.texts()).toContain("System settings");
  expect(app.texts()).not.toContain("系统设置");
  expect(app.storage()["cursor-byok-next:locale"]).toBe("en-US");
  await app.press("id:settings-language");
  await app.press("中文");
  expect(app.texts()).toContain("系统设置");
  expect(app.storage()["cursor-byok-next:locale"]).toBe("zh-CN");
});

test("调用详细页显示 Magpie 记录、详情和清理确认", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(ready, calls, magpieReady, recordedCalls));
  await app.press("调用详细");
  expect(app.texts()).toContain("GPT-5");
  expect(app.texts()).toContain("openai/gpt-5");
  expect(app.texts()).toContain("1 条调用 · 1 条请求 · 1 个响应块");
  expect(app.enabled("清空详情")).toBe(true);
  expect(app.enabled("清空全部")).toBe(true);
  expect(app.texts().join("\n")).not.toContain("Bearer");
  expect(app.texts().join("\n")).not.toContain("sk-");
  await app.press("查看 GPT-5 的详情");
  expect(calls).toContain("detail:call-1");
  const detail = app.texts().join("\n");
  expect(detail).toContain("Call ID");
  expect(detail).toContain("call-1");
  expect(detail).toContain("content-type");
  expect(detail).toContain("data: [DONE]");
  expect(detail).not.toContain("secret");
  await app.press("返回");
  await app.press("清空详情");
  expect(app.texts()).toContain("清空请求和响应详情？");
  await app.press("删除已存详情");
  expect(calls).toContain("clear-details");
});

test("找不到调用时显示独立空状态", async () => {
  const setup = options(ready, [], magpieReady, recordedCalls);
  (setup.host.handlers as Record<string, unknown>)["calls.detail"] = (): CallDetail | null => null;
  const app = await open("./test-fixtures/App.tsx", setup);
  await app.press("调用详细");
  await app.press("查看 GPT-5 的详情");
  expect(app.texts()).toContain("找不到这条调用。");
  expect(app.texts()).not.toContain("暂时无法读取调用详情");
});

test("系统设置页保存端口与清理存储都走真实边界", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(ready, calls, magpieReady, recordedCalls));
  await app.press("系统设置");
  expect(app.texts()).toContain("代理端口");
  expect(app.texts()).toContain("/Users/test/.cursor-byok-next");
  expect(app.texts()).toContain("1 条调用 · 1 条请求 · 1 个响应块");
  expect(app.enabled("保存端口")).toBe(false);
  await app.clear("id:settings-port");
  await app.type("id:settings-port", "41234");
  expect(app.enabled("保存端口")).toBe(true);
  await app.press("保存端口");
  expect(calls).toEqual(["port:41234"]);
  expect(app.texts()).toContain("端口已保存，下次开启接管时生效。");
  await app.press("清空全部");
  expect(app.texts()).toContain("清空全部调用记录？");
  await app.press("删除全部记录");
  expect(calls).toContain("clear-all");
});

test("系统设置页的外观与语言选择立即生效并记住", async () => {
  const app = await open("./test-fixtures/App.tsx", options(ready));
  await app.press("系统设置");
  await app.press("暗色");
  expect(app.storage()["cursor-byok-next:appearance"]).toBe("dark");
  expect(app.texts()).toContain("跟随系统");
  await app.press("id:settings-language");
  await app.press("English");
  expect(app.texts()).toContain("Appearance");
  expect(app.texts()).not.toContain("外观");
  expect(app.storage()["cursor-byok-next:locale"]).toBe("en-US");
});

test("系统设置页在端口越界时给出错误文案", async () => {
  const app = await open("./test-fixtures/App.tsx", options(ready));
  await app.press("系统设置");
  await app.type("id:settings-port", "70000");
  expect(app.texts()).toContain("请填写 0–65535 之间的整数");
  expect(app.enabled("保存端口")).toBe(false);
});

test("清空端口字段表示沿用上次绑定的端口", async () => {
  const calls: string[] = [];
  const chosen = { ...ready, preferredProxyPort: 41234 };
  const app = await open("./test-fixtures/App.tsx", options(chosen, calls));
  await app.press("系统设置");
  expect(app.enabled("保存端口")).toBe(false);
  await app.clear("id:settings-port");
  expect(app.enabled("保存端口")).toBe(true);
  await app.press("保存端口");
  expect(calls).toEqual(["port:null"]);
});

test("调用详细空状态与英文文案", async () => {
  const app = await open("./test-fixtures/App.tsx", options(ready));
  await app.press("调用详细");
  expect(app.texts()).toContain("还没有 Magpie 调用。Cursor 走本地模型后会出现在这里。");
  expect(app.enabled("清空详情")).toBe(false);
  expect(app.enabled("清空全部")).toBe(false);
  await app.press("系统设置");
  await app.press("id:settings-language");
  await app.press("English");
  await app.press("Calls");
  expect(app.texts()).toContain("No Magpie calls yet. They appear here after Cursor uses a local model.");
});
