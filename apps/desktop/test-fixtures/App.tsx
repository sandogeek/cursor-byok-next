import { host, mount, setColorScheme } from "nsh";
import { Window } from "nsh/desktop";
import type { CallDetail, CallsSnapshot, DesktopState, HarnessState, PluginFormResult, PluginSnapshot } from "cursor-byok-next/native";
import { Cursor } from "../src/screens/Cursor";
import { restoreLocale } from "../src/screens/Language";
import { restoreAppearance } from "../src/ui/appearance";
import trayIcon from "./tray.svg";

if (host.params.colorScheme === "light" || host.params.colorScheme === "dark") {
  setColorScheme(host.params.colorScheme);
}

const takeover = {
  status: () => host.call<HarnessState>("cursor.status"),
  initializeCa: () => host.call<HarnessState>("cursor.initializeCa"),
  setEnabled: (enabled: boolean) => host.call<HarnessState>("cursor.setEnabled", { enabled }),
  setPreferredPort: (port: number | null) => host.call<HarnessState>("cursor.setPreferredPort", { port }),
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

const desktop = {
  snapshot: () => host.call<DesktopState>("desktop.snapshot"),
  restore: () => host.call<void>("desktop.restore"),
  setDockIconVisible: (visible: boolean) => host.call<DesktopState>("desktop.setDockIconVisible", { visible }),
  setAppearance: (appearance: string) => host.call<void>("desktop.setAppearance", { appearance }),
};

void restoreAppearance().then(async (appearance) => {
  if (appearance) await desktop.setAppearance(appearance);
}).then(() => restoreLocale()).then(() => {
  mount(() => <Cursor takeover={takeover} plugins={plugins} calls={calls} desktop={desktop} trayIcon={trayIcon} />);
});
