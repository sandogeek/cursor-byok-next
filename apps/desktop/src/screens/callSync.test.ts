import { expect, test } from "nsh/test";
import { SnapshotSync } from "./callSync";

test("旧轮询和卸载后的结果不能覆盖新操作", () => {
  const sync = new SnapshotSync();
  const poll = sync.begin();
  expect(sync.isLatest(poll)).toBe(true);
  const clear = sync.begin();
  expect(sync.isLatest(poll)).toBe(false);
  expect(sync.isLatest(clear)).toBe(true);
  sync.invalidate();
  expect(sync.isLatest(clear)).toBe(false);
});
