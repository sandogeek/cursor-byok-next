import { mount, appInfo } from "nsh";
import { Harness, Plugins, Calls } from "cursor-byok-next/native";
import { Cursor } from "./src/screens/Cursor";
import { restoreLocale } from "./src/screens/Language";
import { checkForUpdates } from "./src/screens/updates";
import { restoreAppearance } from "./src/ui/appearance";

// Native objects own sockets and database handles; keep them for the app's lifetime.
const takeover = new Harness();
const plugins = new Plugins();
const calls = new Calls();

void restoreAppearance().then(() => restoreLocale()).then(() => {
  mount(() => <Cursor takeover={takeover} plugins={plugins} calls={calls} />);
  // A packaged app checks its signed manifest once at launch; a development
  // run resolves `null` without a request, so this costs nothing in `nsh run`.
  if (!appInfo().development) void checkForUpdates();
});
