import { expect, open, test } from "nsh/test";
import { appInfo } from "nsh";

const visualTest = appInfo().name === "Cursor-BYOK-Next UI Tests" ? test : test.skip;

visualTest("明暗主题下均可渲染，弹窗可用 Escape 关闭", async () => {
  for (const colorScheme of ["light", "dark"] as const) {
    const app = await open("./Visual.tsx", {
      width: 1100,
      height: 720,
      platform: "macos",
      locale: "zh-CN" as const,
      host: { params: { colorScheme } },
    });
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}.png`);
    await app.press("安装本地 CA…");
    expect(app.texts()).toContain("安装本地 CA");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-dialog.png`);
    await app.keyboard.press("Escape");
    expect(app.texts()).not.toContain("证书授权需要管理员密码。应用只复制命令，不会替你执行。");
    await app.press("插件配置");
    expect(app.texts()).toContain("无法读取已保存的网关，请检查插件数据文件后重试。");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-plugin-storage.png`);
    await app.press("调用详细");
    expect(app.texts()).toContain("GPT-5");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-calls.png`);
    await app.press("查看 GPT-5 的详情");
    expect(app.texts()).toContain("Call ID");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-call-details.png`);
    await app.press("返回");
    await app.press("系统设置");
    expect(app.texts()).toContain("/Users/test/.cursor-byok-next");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-settings.png`);
    await app.window("main").pressClose();
    expect(app.window("main").state()?.visible).toBe(false);
    await app.host.send("tray.open", {});
    await app.waitUntil(() => app.window("main").state()?.visible === true);
    expect(app.window("main").state()?.focused).toBe(true);
  }
});
