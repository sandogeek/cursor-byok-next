import { List, createMotion, resource, Scroll, signal, Text, View } from "nsh";
import { t } from "nsh/i18n";
import type { PluginFormResult, PluginSnapshot } from "cursor-byok-next/native";
import { Button } from "../ui/Button";
import { Dialog } from "../ui/Dialog";
import { Field } from "../ui/Field";
import { localized } from "../ui/localized";
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
  runtimeError: undefined,
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
  const magpie = () => current().plugins.find((plugin) => plugin.id === "dev.cursorbyoknext.plugins.magpie");
  const provider = () => magpie()?.providers[0];
  const resourceType = () => magpie()?.resources[0];
  const method = () => resourceType()?.add[0];
  const gateways = () => resourceType()?.resources ?? [];
  const models = () => provider()?.models ?? [];
  const runtimeReady = () => current().runtimeReady && (magpie()?.runtimeReady ?? false);
  const storageError = () => resourceType()?.storageError;
  const resourceStatus = (status: string) =>
    status === "ready" ? t("plugins.gateway.ready") : status === "cooling" ? t("plugins.gateway.cooling") : status === "invalid" ? t("plugins.gateway.invalid") : status;

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
      error.set(t("plugins.form.required", { field: localized(missing.label) }));
      return;
    }
    void perform(async () => {
      await props.catalog.submitForm(plugin.id, type.resourceType, add.id, values);
      formValues.set({});
      return props.catalog.snapshot();
    });
  };

  return (
    <View class="min-h-0 flex-1 gap-4">
      <If when={state.error()}>
        <Surface class="gap-2 p-4">
          <Text role="header" class="text-base font-semibold text-foreground">{t("plugins.error.status")}</Text>
          <Text class="text-sm text-muted-foreground">{errorText(state.error())}</Text>
          <Button label={t("common.retry")} variant="primary" loading={state.loading()} onPress={() => state.refetch()} />
        </Surface>
      </If>
      <If when={state.loading() && !runtimeReady()}>
        <Surface class="items-center gap-2 p-8">
          <Text role="header" class="text-base font-semibold text-foreground">{t("plugins.loading")}</Text>
        </Surface>
      </If>
      <If when={!state.loading() && !state.error() && !runtimeReady()}>
        <Surface class="items-center gap-2 p-8">
          <Text role="header" class="text-base font-semibold text-foreground">{current().runtimeReady ? t("plugins.runtime.failed") : t("plugins.runtime.missingBun")}</Text>
          <Text class="max-w-md text-center text-sm text-muted-foreground">{current().runtimeReady ? (current().runtimeError ?? magpie()?.runtimeError ?? t("plugins.runtime.retry")) : t("plugins.runtime.installBun")}</Text>
          <Button label={t("plugins.runtime.refresh")} loading={state.loading()} disabled={busy()} onPress={() => state.refetch()} />
        </Surface>
      </If>
      <If when={!state.error() && runtimeReady() && magpie()}>
        <Scroll class="min-h-0 flex-1" contentClass="gap-4 pb-4">
          <View class="gap-4" style={reveal.style}>
          <Surface class="gap-3 p-4">
            <View class="flex-row items-center justify-between gap-3">
              <View class="gap-1">
                <Text role="header" class="text-base font-semibold text-foreground">{magpie()?.name}</Text>
                <Text class="text-sm text-muted-foreground">{localized(provider()?.description) || t("plugins.magpie.description")}</Text>
              </View>
              <StatusBadge label={storageError() ? t("plugins.status.degraded") : provider()?.configured ? t("plugins.status.configured") : t("plugins.status.unconfigured")} tone={!storageError() && provider()?.configured ? "success" : "neutral"} />
            </View>
            <Each of={method()?.fields ?? []} key={(field) => field.id}>
              {(field) => (
                <Field
                  label={localized(field().label)}
                  value={formValues()[field().id] ?? ""}
                  placeholder={localized(field().placeholder)}
                  masked={field().fieldType === "secret"}
                  keyboardType={field().fieldType === "url" ? "url" : "default"}
                  onChange={(value) => formValues.update((current) => ({ ...current, [field().id]: value }))}
                />
              )}
            </Each>
            <View class="flex-row gap-2">
              <Button label={t("plugins.action.save")} variant="primary" loading={busy()} disabled={!runtimeReady() || state.loading() || !!storageError()} onPress={submit} />
              <Button
                label={t("plugins.action.refreshCatalog")}
                loading={busy()}
                disabled={busy() || state.loading() || !provider()?.configured || !!storageError()}
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
            <Text role="header" class="text-sm font-semibold text-foreground">{t("plugins.gateways.title")}</Text>
            <If when={storageError()}>
              <Text role="alert" class="text-sm text-warning">{storageError()}</Text>
              <Button label={t("plugins.gateways.retry")} loading={state.loading()} disabled={busy()} onPress={() => state.refetch()} />
            </If>
            <If when={!storageError() && gateways().length === 0}>
              <Text class="text-sm text-muted-foreground">{t("plugins.gateways.empty")}</Text>
            </If>
            <Each of={gateways()} key={(item) => item.id}>
              {(item) => (
                <View class="flex-row items-center justify-between gap-3 rounded-(--radius) border border-border px-3 py-2">
                  <View class="min-w-0 gap-1">
                    <Text class="text-sm text-foreground">{item().displayName}</Text>
                    <Text class="text-xs text-muted-foreground">{item().message || resourceStatus(item().status)}</Text>
                  </View>
                  <Button
                    label={t("common.remove")}
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
              <Text role="header" class="text-sm font-semibold text-foreground">{t("plugins.models.title")}</Text>
              <Text class="text-xs text-muted-foreground">{t("plugins.models.count", { count: models().length, note: provider()?.catalogError && models().length > 0 ? t("plugins.models.stale") : t("plugins.models.local") })}</Text>
            </View>
            <If when={models().length === 0 && !storageError() && !provider()?.catalogError}>
              <Text class="text-sm text-muted-foreground">{t("plugins.models.empty")}</Text>
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
        title={t("plugins.dialog.remove.title")}
        description={t("plugins.dialog.remove.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("common.remove")}
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