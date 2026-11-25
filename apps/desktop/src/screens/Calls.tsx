import { List, cleanup, createMotion, signal, Text, View } from "nsh";
import { t } from "nsh/i18n";
import type { CallDetail, CallRecord, CallsSnapshot } from "cursor-byok-next/native";
import { Button, IconButton } from "../ui/Button";
import { Dialog } from "../ui/Dialog";
import { motion } from "../ui/motion";
import { CallStatus, modelLabel, RowButton } from "../ui/Record";
import { Surface } from "../ui/Surface";
import { Switch } from "../ui/Switch";
import { SnapshotSync } from "./callSync";
export interface CallLog {
  snapshot(): Promise<CallsSnapshot>;
  detail(callId: string): Promise<CallDetail | null>;
  setDetailed(enabled: boolean): Promise<CallsSnapshot>;
  clearDetails(): Promise<CallsSnapshot>;
  clearAll(): Promise<CallsSnapshot>;
}

const emptySnapshot: CallsSnapshot = {
  calls: [],
  detailed: false,
  storage: { callCount: 0, requestCount: 0, chunkCount: 0 },
  degraded: false,
};

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

function formatTime(ms: number) {
  return new Date(ms).toLocaleString();
}

function formatMs(value: number | undefined) {
  return value == null ? t("calls.none") : `${value} ms`;
}

function formatCount(value: number | undefined) {
  return value == null ? t("calls.none") : String(value);
}

export function Calls(props: { log: CallLog; onOpen: (callId: string) => void }) {
  const state = signal(emptySnapshot);
  const loading = signal(true);
  const readError = signal("");
  const busy = signal(false);
  const error = signal("");
  const confirm = signal<"details" | "all" | null>(null);
  let active = true;
  const sync = new SnapshotSync();
  let refreshing = false;
  let initialized = false;
  const reveal = createMotion({
    enter: { keyframes: [{ opacity: 0, translateY: motion.distance }, { opacity: 1, translateY: 0 }], duration: motion.base, easing: "easeOut" },
  });
  const current = () => state();
  const calls = () => current().calls;

  const perform = async (operation: () => Promise<CallsSnapshot>) => {
    if (busy()) return;
    const ownRevision = sync.begin();
    busy.set(true);
    error.set("");
    readError.set("");
    try {
      const snapshot = await operation();
      if (active && sync.isLatest(ownRevision)) {
        state.set(snapshot);
        busy.set(false);
      }
    } catch (cause) {
      if (active && sync.isLatest(ownRevision)) {
        error.set(errorText(cause));
        busy.set(false);
      }
    }
  };

  const refresh = (manual = false) => {
    if (refreshing || !active || (!manual && busy())) return;
    const ownRevision = sync.begin();
    refreshing = true;
    loading.set(!initialized);
    readError.set("");
    void props.log.snapshot().then((snapshot) => {
      if (active && sync.isLatest(ownRevision)) state.set(snapshot);
    }).catch((cause: unknown) => {
      if (active && sync.isLatest(ownRevision)) readError.set(errorText(cause));
    }).finally(() => {
      refreshing = false;
      if (active && sync.isLatest(ownRevision)) {
        loading.set(false);
        initialized = true;
      }
    });
  };
  void refresh();
  const id = setInterval(() => void refresh(), 2000);
  cleanup(() => {
    active = false;
    sync.invalidate();
    clearInterval(id);
  });

  return (
    <View class="min-h-0 flex-1 gap-4">
      <If when={readError()}>
        <Surface class="gap-2 p-4">
          <Text role="header" class="text-base font-semibold text-foreground">{t("calls.error.status")}</Text>
          <Text class="text-sm text-muted-foreground">{readError()}</Text>
          <Button label={t("common.retry")} variant="primary" loading={loading()} onPress={() => void refresh(true)} />
        </Surface>
      </If>
      <If when={!readError()}>
        <View class="min-h-0 flex-1 gap-4" style={reveal.style}>
          <Surface class="gap-3 p-4">
            <View class="flex-row items-center justify-between gap-3">
              <View class="gap-1">
                <Text class="text-sm text-muted-foreground">{t("calls.count", { calls: current().storage.callCount, requests: current().storage.requestCount, chunks: current().storage.chunkCount })}</Text>
                <If when={current().degraded}>
                  <Text role="alert" class="text-sm text-warning">{t("calls.degraded")}</Text>
                </If>
              </View>
              <View class="flex-row items-center gap-2">
                <Text class="text-xs text-muted-foreground">{t("calls.detailed")}</Text>
                <Switch
                  checked={current().detailed}
                  label={t("calls.detailed")}
                  disabled={busy() || loading()}
                  onChange={({ checked }) => void perform(() => props.log.setDetailed(checked))}
                />
                <IconButton label={t("calls.refresh")} icon="↻" loading={loading()} disabled={busy()} onPress={() => void refresh(true)} />
              </View>
            </View>
            <Text class="text-xs text-muted-foreground">{t("calls.detailedHint")}</Text>
            <View class="flex-row gap-2">
              <Button label={t("calls.clearDetails")} disabled={busy() || (current().storage.requestCount === 0 && current().storage.chunkCount === 0)} onPress={() => confirm.set("details")} />
              <Button label={t("calls.clearAll")} disabled={busy() || current().storage.callCount === 0} onPress={() => confirm.set("all")} />
            </View>
            <If when={error()}>
              <Text role="alert" class="text-sm text-warning">{error()}</Text>
            </If>
          </Surface>

          <Surface class="min-h-0 flex-1 gap-2 p-4">
            <View class="flex-row items-center gap-2 px-3">
              <Text class="w-24 shrink-0 text-xs text-muted-foreground">{t("calls.columns.status")}</Text>
              <Text class="min-w-40 flex-1 text-xs text-muted-foreground">{t("calls.columns.model")}</Text>
              <Text class="w-12 shrink-0 text-end text-xs text-muted-foreground">{t("calls.columns.http")}</Text>
              <Text class="w-16 shrink-0 text-end text-xs text-muted-foreground">{t("calls.columns.duration")}</Text>
              <Text class="w-12 shrink-0 text-end text-xs text-muted-foreground">{t("calls.columns.input")}</Text>
              <Text class="w-12 shrink-0 text-end text-xs text-muted-foreground">{t("calls.columns.output")}</Text>
              <Text class="w-12 shrink-0 text-end text-xs text-muted-foreground">{t("calls.columns.total")}</Text>
              <Text class="w-28 shrink-0 text-xs text-muted-foreground">{t("calls.columns.error")}</Text>
            </View>
            <If when={loading() && calls().length === 0}>
              <Text class="px-3 py-8 text-sm text-muted-foreground">{t("calls.loading")}</Text>
            </If>
            <If when={!loading() && calls().length === 0}>
              <Text class="px-3 py-8 text-sm text-muted-foreground">{t("calls.empty")}</Text>
            </If>
            <If when={calls().length > 0}>
              <List each={calls()} keyExtractor={(call: CallRecord) => call.callId} estimatedItemSize={56} class="min-h-0 flex-1">
                {(call) => (
                  <RowButton
                    accessibilityLabel={t("calls.viewNamed", { name: call().displayName })}
                    onPress={() => props.onOpen(call().callId)}
                  >
                    <View class="w-24 shrink-0 flex-row items-center">
                      <CallStatus status={call().status} />
                    </View>
                    <View class="min-w-40 flex-1 overflow-hidden gap-0.5">
                      <Text class="truncate text-sm text-foreground">{call().displayName}</Text>
                      <Text class="truncate font-mono text-xs text-muted-foreground">{modelLabel(call().modelId)}</Text>
                      <Text class="truncate text-xs text-muted-foreground">{formatTime(call().createdAtMs)}</Text>
                    </View>
                    <Text class="w-12 shrink-0 text-end font-mono text-xs tabular-nums text-muted-foreground">{formatCount(call().httpStatus ?? undefined)}</Text>
                    <Text class="w-16 shrink-0 text-end font-mono text-xs tabular-nums text-muted-foreground">{formatMs(call().durationMs ?? undefined)}</Text>
                    <Text class="w-12 shrink-0 text-end font-mono text-xs tabular-nums text-muted-foreground">{formatCount(call().inputTokens ?? undefined)}</Text>
                    <Text class="w-12 shrink-0 text-end font-mono text-xs tabular-nums text-muted-foreground">{formatCount(call().outputTokens ?? undefined)}</Text>
                    <Text class="w-12 shrink-0 text-end font-mono text-xs tabular-nums text-muted-foreground">{formatCount(call().totalTokens ?? undefined)}</Text>
                    <Text class={`w-28 shrink-0 truncate text-xs ${call().errorMessage ? "text-warning" : "text-muted-foreground"}`}>{call().errorMessage || t("calls.none")}</Text>
                  </RowButton>
                )}
              </List>
            </If>
          </Surface>
        </View>
      </If>

      <Dialog
        open={confirm() === "details"}
        title={t("calls.dialog.details.title")}
        description={t("calls.dialog.details.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("calls.dialog.details.confirm")}
        onCancel={() => confirm.set(null)}
        onConfirm={() => {
          confirm.set(null);
          void perform(() => props.log.clearDetails());
        }}
      />
      <Dialog
        open={confirm() === "all"}
        title={t("calls.dialog.all.title")}
        description={t("calls.dialog.all.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("calls.dialog.all.confirm")}
        onCancel={() => confirm.set(null)}
        onConfirm={() => {
          confirm.set(null);
          void perform(() => props.log.clearAll());
        }}
      />
    </View>
  );
}
