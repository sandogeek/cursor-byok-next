import { expect, test } from "nsh/test";
import { modelLabel } from "./Record";

test("只去掉资源绑定前缀,模型自身的层级保留", () => {
  expect(modelLabel("2eba461c-6882-4040-95c7-b39b98d1fde5/dumate/deepseek-v41-flash")).toBe("dumate/deepseek-v41-flash");
  expect(modelLabel("openai/gpt-5")).toBe("openai/gpt-5");
  expect(modelLabel("not-a-uuid/dumate/model")).toBe("not-a-uuid/dumate/model");
});
