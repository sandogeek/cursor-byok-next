// Settings' update card and the check that runs at launch: the release
// manifest is not reachable in a test run, so the states are driven from the
// mirror the card reads.

import { beforeEach, expect, test } from "nsh/test";
import { updateState, type UpdateState } from "./updates";

const initial: UpdateState = { status: "idle", update: null, received: 0, total: null, error: "" };

function shown(state: UpdateState) {
  return `${state.status}:${state.update?.version ?? "-"}`;
}

beforeEach(() => updateState.set(initial));

test("a development run has no update to offer", () => {
  expect(shown(updateState())).toBe("idle:-");
});

test("an offered release carries its notes and install permission", () => {
  updateState.set({
    status: "available",
    update: { version: "1.1.0", notes: "Fixes", url: "https://example.com/a.zip", canInstall: true, shell: false },
    received: 0,
    total: null,
    error: "",
  });
  expect(shown(updateState())).toBe("available:1.1.0");
  expect(updateState().update?.canInstall).toBe(true);
});

test("a download reports bytes against the manifest's size", () => {
  updateState.set({
    status: "downloading",
    update: { version: "1.1.0", url: "https://example.com/a.zip", canInstall: true, shell: true },
    received: 1024,
    total: 4096,
    error: "",
  });
  expect(updateState().received).toBe(1024);
  expect(updateState().total).toBe(4096);
  expect(updateState().update?.shell).toBe(true);
});

test("a failed check keeps the message the card shows", () => {
  updateState.set({ ...initial, status: "failed", error: "manifest: signature does not match" });
  expect(updateState().error).toContain("signature");
});