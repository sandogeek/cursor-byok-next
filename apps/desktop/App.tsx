import { mount } from "nsh";
import { Harness, Plugins, Calls } from "cursor-byok-next/native";
import { Cursor } from "./src/screens/Cursor";
import { restoreLocale } from "./src/screens/Language";

void restoreLocale().then(() => {
  mount(() => <Cursor takeover={new Harness()} plugins={new Plugins()} calls={new Calls()} />);
});