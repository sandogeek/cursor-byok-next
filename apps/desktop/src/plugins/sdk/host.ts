/** The only native capability bridge. All payloads are small JSON strings. */
declare const __host: (requestId: string, method: string, params: string) => Promise<string>;
declare const __readRequest: (requestId: string) => string;
export const fetch = (id: string, url: string, init: string) => __host(id, "network.fetch", JSON.stringify({url, ...JSON.parse(init)}));
export const streamOpen = (id: string, url: string, init: string) => __host(id, "network.stream.open", JSON.stringify({url, ...JSON.parse(init)}));
export const streamRead = (id: string, streamId: string) => __host(id, "network.stream.read", JSON.stringify({streamId}));
export const streamClose = (id: string, streamId: string) => __host(id, "network.stream.close", JSON.stringify({streamId})).then(() => null);
export const protocolStream = (id: string, input: string) => __host(id, "protocol.stream", input);
export const readRequest = (id: string): string => __readRequest(id);
