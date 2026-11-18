import { argv } from "node:process";
import { __descriptor, __getRegisteredPlugin } from "cursor-byok-next:plugin";

const args = argv.slice(2);
if (args.length !== 1) throw new Error("plugin entry URL is required");
await import(args[0]);
console.log("CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:" + JSON.stringify(__descriptor(__getRegisteredPlugin())));
