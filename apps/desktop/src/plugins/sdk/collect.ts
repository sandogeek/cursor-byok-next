import { __descriptor, __getRegisteredPlugin } from "cursor-byok-next:plugin";
declare function __entry(): string;
declare function __send(frame: string): void;
await import(__entry());
__send(JSON.stringify(__descriptor(__getRegisteredPlugin())));
