import { List, createMotion, resource, Scroll, signal, Text, View } from "nsh";
import type { PluginFormResult, PluginSnapshot } from "cursor-byok/native";
import { Button } from "../ui/Button";
import { Dialog } from "../ui/Dialog";
import { Field } from "../ui/Field";
import { motion } from "../ui/motion";
import { StatusBadge, Surface } from "../ui/Surface";

export interface PluginCatalog {
  snapshot(): Promise<PluginSnapshot>;
  submitForm(
    pluginId: string,
    resourceType: string,
    methodId: string,
    values: Array<{ id: string; value: string }>,
  ): Promise<PluginFormResult>;
  deleteResource(pluginId: string, resourceType: string, resourceId: string): Promise<PluginSnapshot>;
  refreshCatalog(pluginId: string, providerId: string): Promise<PluginSnapshot>;
}

const emptySnapshot: PluginSnapshot = {
  runtimeReady: false,
  runtimeError: "正在读取插件状态…",
  plugins: [],
};

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

export function Plugins(props: { catalog: PluginCatalog }) {
  const state = resource(() => props.catalog.snapshot(), { initialValue: emptySnapshot });
  const busy = signal(false);
  const error = signal("");
  const formValues = signal<Record<string, string>>({});
  const removing = signal<string | null>(null);
  const reveal = createMotion({
    enter: { keyframes: [{ opacity: 0, translateY: motion.distance }, { opacity: 1, translateY: 0 }], duration: motion.base, easing: "easeOut" },
  });
  const current = () => state() ?? emptySnapshot;
  const magpie = () => current().plugins.find((plugin) => plugin.id === "dev.cursorbyok.plugins.magpie");
  const provider = () => magpie()?.providers[0];
  const resourceType = () => magpie()?.resources[0];
  const method = () => resourceType()?.add[0];
  const gateways = () => resourceType()?.resources ?? [];
  const models = () => provider()?.models ?? [];
  const runtimeReady = () => current().runtimeReady && (magpie()?.runtimeReady ?? false);

  const perform = async (operation: () => Promise<PluginSnapshot>) => {
    if (busy()) return;
    busy.set(true);
    error.set("");
    try {
      state.mutate(await operation());
    } catch (cause) {
      error.set(errorText(cause));
      state.refetch();
    } finally {
      busy.set(false);
    }
  };

  const submit = () => {
    const plugin = magpie();
    const type = resourceType();
    const add = method();
    if (!plugin || !type || !add) return;
    const values = add.fields.map((field) => ({
      id: field.id,
      value: (formValues()[field.id] ?? "").trim(),
    }));
    const missing = add.fields.find((field) => field.required && !values.find((value) => value.id === field.id)?.value);
    if (missing) {
      error.set(`请填写${missing.label}`);
      return;
    }
    void perform(async () => {
      const result = await props.catalog.submitForm(plugin.id, type.resourceType, add.id, values);
      if (result.modelSyncError) error.set(result.modelSyncError);
      formValues.set({});
      return props.catalog.snapshot();
    });
  };

  return (
    <View class="min-h-0 flex-1 gap-4">
      <If when={state.error()}>
        <Surface class="gap-2 p-4">
          <Text role="header" class="text-base font-semibold text-foreground">暂时无法读取插件状态</Text>
          <Text class="text-sm text-muted-foreground">{errorText(state.error())}</Text>
          <Button label="重试" variant="primary" loading={state.loading()} onPress={() => state.refetch()} />
        </Surface>
      </If>
      <If when={!state.error() && !runtimeReady()}>
        <Surface class="items-center gap-2 p-8">
          <Text role="header" class="text-base font-semibold text-foreground">需要 Bun 才能加载 Magpie 插件</Text>
          <Text class="max-w-md text-center text-sm text-muted-foreground">{current().runtimeError ?? magpie()?.runtimeError ?? "把 bun 放到 PATH 后刷新。"}</Text>
          <Button label="刷新状态" loading={state.loading()} disabled={busy()} onPress={() => state.refetch()} />
        </Surface>
      </If>
      <If when={!state.error() && runtimeReady() && magpie()}>
        <Scroll class="min-h-0 flex-1" contentClass="gap-4 pb-4">
          <View class="gap-4" style={reveal.style}>
          <Surface class="gap-3 p-4">
            <View class="flex-row items-center justify-between gap-3">
              <View class="gap-1">
                <Text role="header" class="text-base font-semibold text-foreground">{magpie()?.name}</Text>
                <Text class="text-sm text-muted-foreground">{provider()?.description || "从 Magpie 网关读取实时模型目录。"}</Text>
              </View>
              <StatusBadge label={provider()?.configured ? "已连接" : "未连接"} tone={provider()?.configured ? "success" : "neutral"} />
            </View>
            <Each of={method()?.fields ?? []} key={(field) => field.id}>
              {(field) => (
                <Field
                  label={field().label}
                  value={formValues()[field().id] ?? ""}
                  placeholder={field().placeholder}
                  masked={field().fieldType === "secret"}
                  keyboardType={field().fieldType === "url" ? "url" : "default"}
                  onChange={(value) => formValues.update((current) => ({ ...current, [field().id]: value }))}
                />
              )}
            </Each>
            <View class="flex-row gap-2">
              <Button label="保存连接" variant="primary" loading={busy()} disabled={!runtimeReady()} onPress={submit} />
              <Button
                label="刷新目录"
                loading={busy()}
                disabled={busy() || !provider()?.configured}
                onPress={() => {
                  const plugin = magpie();
                  const next = provider();
                  if (!plugin || !next) return;
                  void perform(() => props.catalog.refreshCatalog(plugin.id, next.id));
                }}
              />
            </View>
            <If when={provider()?.catalogError}>
              <Text role="alert" class="text-sm text-warning">{provider()?.catalogError}</Text>
            </If>
          </Surface>

          <Surface class="gap-3 p-4">
            <Text role="header" class="text-sm font-semibold text-foreground">网关</Text>
            <If when={gateways().length === 0}>
              <Text class="text-sm text-muted-foreground">还没有 Magpie 网关。填写地址和 API 密钥后保存。</Text>
            </If>
            <Each of={gateways()} key={(item) => item.id}>
              {(item) => (
                <View class="flex-row items-center justify-between gap-3 rounded-(--radius) border border-border px-3 py-2">
                  <View class="min-w-0 gap-1">
                    <Text class="text-sm text-foreground">{item().displayName}</Text>
                    <Text class="text-xs text-muted-foreground">{item().status === "ready" ? "可用" : item().message || item().status}</Text>
                  </View>
                  <Button
                    label="移除"
                    size="sm"
                    disabled={busy()}
                    onPress={() => removing.set(item().id)}
                  />
                </View>
              )}
            </Each>
          </Surface>

          <Surface class="min-h-48 gap-3 p-4">
            <View class="flex-row items-center justify-between">
              <Text role="header" class="text-sm font-semibold text-foreground">模型目录</Text>
              <Text class="text-xs text-muted-foreground">{models().length} 个模型 · 仅本页可见</Text>
            </View>
            <If when={models().length === 0}>
              <Text class="text-sm text-muted-foreground">连接网关并刷新后，Magpie 模型会出现在这里。Cursor 选择器暂不接入。</Text>
            </If>
            <If when={models().length > 0}>
              <List each={models()} estimatedItemSize={56} class="min-h-40">
                {(model) => (
                  <View class="gap-1 border-b border-border py-2">
                    <Text class="text-sm text-foreground">{model().displayName}</Text>
                    <Text class="select-text font-mono text-xs text-muted-foreground">{model().id}</Text>
                  </View>
                )}
              </List>
            </If>
          </Surface>
          <If when={error()}>
            <Text role="alert" class="text-sm text-warning">{error()}</Text>
          </If>
          </View>
        </Scroll>
      </If>

      <Dialog
        open={removing() !== null}
        title="移除 Magpie 网关？"
        description="只会删除本应用保存的网关凭证和目录缓存，不影响 Cursor 官方配置。"
        cancelLabel="取消"
        confirmLabel="移除"
        onCancel={() => removing.set(null)}
        onConfirm={() => {
          const plugin = magpie();
          const type = resourceType();
          const id = removing();
          removing.set(null);
          if (!plugin || !type || !id) return;
          void perform(() => props.catalog.deleteResource(plugin.id, type.resourceType, id));
        }}
      />
    </View>
  );
}
