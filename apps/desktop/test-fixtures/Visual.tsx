import { host, mount, setColorScheme } from "nsh";
import type { WindowRef } from "nsh/desktop";
import type { CallDetail, CallRecord, CallsSnapshot, HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok-next/native";
import { Cursor } from "../src/screens/Cursor";
import { restoreLocale } from "../src/screens/Language";
import trayIcon from "./tray.svg";

if (host.params.colorScheme === "light" || host.params.colorScheme === "dark") {
  setColorScheme(host.params.colorScheme);
}

const state: HarnessState = {
  platform: "macos",
  ca: "untrusted",
  integration: "disabled",
  settingsApplied: false,
  preferredProxyPort: null,
  dataDir: "/Users/test/.cursor-byok-next",
  caInstallCommand: "sudo security add-trusted-cert placeholder-ca.crt",
};
const takeover = {
  status: async () => state,
  initializeCa: async () => state,
  setEnabled: async () => state,
  setPreferredPort: async (_port: number | null) => state,
};
const plugins = {
  snapshot: async (): Promise<PluginSnapshot> => ({
    runtimeReady: true,
    plugins: [{
      id: "dev.cursorbyoknext.plugins.magpie",
      name: "Magpie",
      version: "0.1.0",
      icon: "",
      runtimeReady: true,
      providers: [{ id: "magpie", pluginId: "dev.cursorbyoknext.plugins.magpie", displayName: "Magpie", description: { "en-US": "Live catalog from a Magpie gateway.", "zh-CN": "从 Magpie 网关实时读取目录。" }, providerType: "magpie", catalog: "live", configured: false, models: [] }],
      resources: [{ resourceType: "gateway", displayName: { "en-US": "Gateways", "zh-CN": "网关" }, add: [], storageError: "无法读取已保存的网关，请检查插件数据文件后重试。", resources: [] }],
    }],
  }),
  submitForm: async (): Promise<PluginFormResult> => ({ added: 0, updated: 0 }),
  deleteResource: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
  refreshCatalog: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
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
const recordedCalls: CallsSnapshot = {
  calls: [sampleCall],
  detailed: true,
  storage: { callCount: 1, requestCount: 1, chunkCount: 1 },
  degraded: false,
};
const calls = {
  snapshot: async (): Promise<CallsSnapshot> => recordedCalls,
  detail: async (): Promise<CallDetail | null> => ({
    call: sampleCall,
    request: {
      headersJson: "{\"content-type\":\"application/json\"}",
      bodyJson: "{\"model\":\"openai/gpt-5\"}",
      byteCount: 12,
    },
    chunks: [{ seq: 0, receivedOffsetMs: 12, data: "data: [DONE]\\n\\n", byteCount: 14 }],
  }),
  setDetailed: async (): Promise<CallsSnapshot> => recordedCalls,
  clearDetails: async (): Promise<CallsSnapshot> => recordedCalls,
  clearAll: async (): Promise<CallsSnapshot> => recordedCalls,
};

const desktop = {
  snapshot: async () => ({ showDockIcon: true }),
  restore: async () => {},
  setDockIconVisible: async () => ({ showDockIcon: true }),
};

void restoreLocale().then(() => {
  let window: WindowRef | undefined;
  mount(() => <Cursor takeover={takeover} plugins={plugins} calls={calls} desktop={desktop} trayIcon={trayIcon} onWindow={(value) => { window = value; }} />);
  host.on("tray.open", () => window?.focus());
});