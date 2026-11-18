import { argv } from "node:process";
import { __descriptor, __getRegisteredPlugin } from "cursor-byok:plugin";

const args = argv.slice(2);
if (args.length !== 1) throw new Error("plugin entry URL is required");
await import(args[0]);
console.log("CURSOR_BYOK_PLUGIN_DEFINITION:" + JSON.stringify(__descriptor(__getRegisteredPlugin())));
