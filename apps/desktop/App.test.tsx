import { expect, open, test } from "nsh/test";
import type { HarnessState } from "cursor-byok/native";

const missing: HarnessState = {
  platform: "macos",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
};
const untrusted: HarnessState = {
  ...missing,
  ca: "untrusted",
  caInstallCommand: "sudo security add-trusted-cert placeholder-ca.crt",
};
const ready: HarnessState = { ...missing, ca: "ready" };
const enabled: HarnessState = {
  ...ready,
  integration: "enabled",
  settingsApplied: true,
  proxyUrl: "http://127.0.0.1:39831",
  proxyPort: 39831,
};

function options(initial: HarnessState, calls: string[] = []) {
  let current = initial;
  return {
    width: 1100,
    height: 720,
    platform: "macos" as const,
    host: {
      handlers: {
        "cursor.status": () => current,
        "cursor.initializeCa": () => {
          calls.push("initializeCa");
          current = untrusted;
          return current;
        },
        "cursor.setEnabled": ({ enabled: next }: { enabled: boolean }) => {
          calls.push(next ? "enable" : "disable");
          current = next ? enabled : ready;
          return current;
        },
      },
    },
    modules: {
      "nsh": {
        "Clipboard.writeText": async (text: string) => { calls.push(`copy:${text}`); },
      },
    },
  };
}

test("复刻接管壳，CA 未信任时不能开启接管", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(missing, calls));
  expect(app.texts()).toContain("数据概览");
  expect(app.texts()).toContain("调用详细");
  expect(app.texts()).toContain("模型配置");
  expect(app.texts()).toContain("插件配置");
  expect(app.texts()).toContain("需要先初始化本地 CA");
  expect(app.enabled("开启接管 Cursor")).toBe(false);
  await app.press("初始化 CA");
  expect(app.texts()).toContain("安装本地 CA");
  expect(calls).toEqual(["initializeCa"]);
  await app.press("复制命令");
  expect(app.texts()).toContain("命令已复制");
  expect(calls[1]?.startsWith("copy:sudo security")).toBe(true);
});

test("关闭接管先确认，再调用真实状态边界", async () => {
  const calls: string[] = [];
  const app = await open("./test-fixtures/App.tsx", options(enabled, calls));
  expect(app.texts()).toContain("已接管");
  expect(app.texts()).toContain("Cursor 接管已生效");
  await app.press("关闭接管 Cursor");
  expect(app.texts()).toContain("关闭接管 Cursor？");
  expect(calls).toEqual([]);
  await app.press("取消");
  expect(calls).toEqual([]);
  await app.press("关闭接管 Cursor");
  await app.press("关闭接管");
  expect(calls).toEqual(["disable"]);
  expect(app.texts()).toContain("本地 CA 已就绪");
});

test("代理与设置不一致时显示降级状态", async () => {
  const app = await open("./test-fixtures/App.tsx", options({ ...ready, integration: "degraded", proxyUrl: "http://127.0.0.1:39831" }));
  expect(app.texts()).toContain("状态异常");
  expect(app.texts()).toContain("本地代理与 Cursor 配置不一致，请刷新状态或重新开启接管。");
});
