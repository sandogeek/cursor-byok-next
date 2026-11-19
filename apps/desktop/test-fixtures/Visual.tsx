import { host, mount, setColorScheme } from "nsh";
import type { HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok-next/native";
import { Cursor } from "../src/screens/Cursor";

if (host.params.colorScheme === "light" || host.params.colorScheme === "dark") {
  setColorScheme(host.params.colorScheme);
}

const state: HarnessState = {
  platform: "macos",
  ca: "untrusted",
  integration: "disabled",
  settingsApplied: false,
  caInstallCommand: "sudo security add-trusted-cert placeholder-ca.crt",
};
const takeover = {
  status: async () => state,
  initializeCa: async () => state,
  setEnabled: async () => state,
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
      providers: [{ id: "magpie", pluginId: "dev.cursorbyoknext.plugins.magpie", displayName: "Magpie", description: "从 Magpie 网关实时读取目录。", providerType: "magpie", catalog: "live", configured: false, models: [] }],
      resources: [{ resourceType: "gateway", displayName: "网关", add: [], storageError: "无法读取已保存的网关，请检查插件数据文件后重试。", resources: [] }],
    }],
  }),
  submitForm: async (): Promise<PluginFormResult> => ({ added: 0, updated: 0 }),
  deleteResource: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
  refreshCatalog: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
};

mount(() => <Cursor takeover={takeover} plugins={plugins} />);
