import { mount } from "nsh";
import { Harness, Plugins } from "cursor-byok-next/native";
import { Cursor } from "./src/screens/Cursor";

mount(() => <Cursor takeover={new Harness()} plugins={new Plugins()} />);
