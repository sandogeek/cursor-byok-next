import { expect, open, test } from "nsh/test";
import { appInfo } from "nsh";

const visualTest = appInfo().name === "Cursor-BYOK-Next UI Tests" ? test : test.skip;

visualTest("明暗主题下均可渲染，弹窗可用 Escape 关闭", async () => {
  for (const colorScheme of ["light", "dark"] as const) {
    const app = await open("./Visual.tsx", {
      width: 1100,
      height: 720,
      platform: "macos",
      host: { params: { colorScheme } },
    });
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}.png`);
    await app.press("安装本地 CA…");
    expect(app.texts()).toContain("安装本地 CA");
    await app.screenshot(`/tmp/cursor-byok-next-${colorScheme}-dialog.png`);
    await app.keyboard.press("Escape");
    expect(app.texts()).not.toContain("证书授权需要管理员密码。应用只复制命令，不会替你执行。");
  }
});
