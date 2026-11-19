import { expect, open, test } from "nsh/test";
import type { HarnessState, PluginSnapshot } from "cursor-byok-next/native";

const missing: HarnessState = {
  platform: "macos",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
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

function options(initial: HarnessState, calls: string[] = [], plugins: PluginSnapshot = { runtimeReady: false, runtimeError: "bun is not on PATH", plugins: [] }) {
  let current = initial;
  let catalog = plugins;
  return {
    width: 1100,
    height: 720,
    platform: "macos" as const,
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
