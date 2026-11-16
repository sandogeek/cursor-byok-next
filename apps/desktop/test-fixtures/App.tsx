import { host, mount, setColorScheme } from "nsh";
import type { HarnessState } from "cursor-byok/native";
import { Cursor } from "../src/screens/Cursor";

if (host.params.colorScheme === "light" || host.params.colorScheme === "dark") {
  setColorScheme(host.params.colorScheme);
}

const takeover = {
  status: () => host.call<HarnessState>("cursor.status"),
  initializeCa: () => host.call<HarnessState>("cursor.initializeCa"),
  setEnabled: (enabled: boolean) => host.call<HarnessState>("cursor.setEnabled", { enabled }),
};

mount(() => <Cursor takeover={takeover} />);
