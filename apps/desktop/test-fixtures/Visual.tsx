import { host, mount, setColorScheme } from "nsh";
import type { HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok/native";
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
    runtimeReady: false,
    runtimeError: "bun is not on PATH",
    plugins: [],
  }),
  submitForm: async (): Promise<PluginFormResult> => ({ added: 0, updated: 0 }),
  deleteResource: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
  refreshCatalog: async (): Promise<PluginSnapshot> => ({ runtimeReady: false, plugins: [] }),
};

mount(() => <Cursor takeover={takeover} plugins={plugins} />);
