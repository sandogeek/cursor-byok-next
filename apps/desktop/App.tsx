import { mount, appInfo, platform } from "nsh";
import { Harness, Plugins, Calls, Desktop } from "cursor-byok-next/native";
import { Cursor } from "./src/screens/Cursor";
import { restoreLocale } from "./src/screens/Language";
import { checkForUpdates } from "./src/screens/updates";
import { restoreAppearance } from "./src/ui/appearance";
import trayIcon from "./assets/tray.svg";

// Native objects own sockets and database handles; keep them for the app's lifetime.
const takeover = new Harness();
const plugins = new Plugins();
const calls = new Calls();
const desktop = new Desktop();

void restoreAppearance().then(async (appearance) => {
  // A fixed appearance also reaches the title bar the platform draws; the
  // screens took it already. Following the system is AppKit's default.
  if (appearance) await desktop.setAppearance(appearance);
}).then(() => restoreLocale()).then(async () => {
  mount(() => <Cursor takeover={takeover} plugins={plugins} calls={calls} desktop={desktop} trayIcon={trayIcon} />);
  if (platform.os === "macos") await desktop.restore();
  // A packaged app checks its signed manifest once at launch; a development
  // run resolves `null` without a request, so this costs nothing in `nsh run`.
  if (!appInfo().development) void checkForUpdates();
});
