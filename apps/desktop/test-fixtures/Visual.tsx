import { host, mount, setColorScheme } from "nsh";
import type { HarnessState } from "cursor-byok/native";
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

mount(() => <Cursor takeover={takeover} />);
