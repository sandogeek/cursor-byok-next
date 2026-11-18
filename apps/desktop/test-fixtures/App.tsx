import { host, mount, setColorScheme } from "nsh";
import type { HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok/native";
import { Cursor } from "../src/screens/Cursor";

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

mount(() => <Cursor takeover={takeover} plugins={plugins} />);
