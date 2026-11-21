import { host, mount, setColorScheme } from "nsh";
import type { CallDetail, CallsSnapshot, HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok-next/native";
import { Cursor } from "../src/screens/Cursor";
import { restoreLocale } from "../src/screens/Language";

if (host.params.colorScheme === "light" || host.params.colorScheme === "dark") {
  setColorScheme(host.params.colorScheme);
}

const takeover = {
  status: () => host.call<HarnessState>("cursor.status"),
  initializeCa: () => host.call<HarnessState>("cursor.initializeCa"),
  setEnabled: (enabled: boolean) => host.call<HarnessState>("cursor.setEnabled", { enabled }),
};

const plugins = {
  snapshot: () => host.call<PluginSnapshot>("plugins.snapshot"),
  submitForm: (
    pluginId: string,
    resourceType: string,
    methodId: string,
    values: Array<{ id: string; value: string }>,
  ) => host.call<PluginFormResult>("plugins.submitForm", { pluginId, resourceType, methodId, values }),
  deleteResource: (pluginId: string, resourceType: string, resourceId: string) =>
    host.call<PluginSnapshot>("plugins.deleteResource", { pluginId, resourceType, resourceId }),
  refreshCatalog: (pluginId: string, providerId: string) =>
    host.call<PluginSnapshot>("plugins.refreshCatalog", { pluginId, providerId }),
};

const calls = {
  snapshot: () => host.call<CallsSnapshot>("calls.snapshot"),
  detail: (callId: string) => host.call<CallDetail | null>("calls.detail", { callId }),
  setDetailed: (enabled: boolean) => host.call<CallsSnapshot>("calls.setDetailed", { enabled }),
  clearDetails: () => host.call<CallsSnapshot>("calls.clearDetails"),
  clearAll: () => host.call<CallsSnapshot>("calls.clearAll"),
};

void restoreLocale().then(() => {
  mount(() => <Cursor takeover={takeover} plugins={plugins} calls={calls} />);
});
