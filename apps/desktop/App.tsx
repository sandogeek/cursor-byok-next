import { mount } from "nsh";
import { Harness } from "cursor-byok/native";
import { Cursor } from "./src/screens/Cursor";

mount(() => <Cursor takeover={new Harness()} />);
