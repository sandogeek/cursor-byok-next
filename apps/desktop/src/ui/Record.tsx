import { Clipboard, Scroll, signal, Text, View, type JSX } from "nsh";
import type { CallStatusView } from "cursor-byok-next/native";
import { t } from "nsh/i18n";
import { Button as BaseButton } from "nsh/base";
import { Button } from "./Button";
import { colorMotion, controlMotion } from "./motion";
import { StatusBadge } from "./Surface";

const LIVE_RESOURCE_PREFIX = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\//i;

/**
 * The model id without the live-catalog resource id `PluginHost` binds in front
 * of it (`state` generates those ids as UUIDs); `modelHash` on the details
 * screen still carries the whole id.
 */
export function modelLabel(modelId: string) {
  return modelId.replace(LIVE_RESOURCE_PREFIX, "");
}

export function FieldRow(props: { label: string; value: string; mono?: boolean }) {
  return (
    <View class="flex-row items-start justify-between gap-4 border-b border-border py-2">
      <Text class="w-36 shrink-0 text-xs text-muted-foreground">{props.label}</Text>
      <Text class={`min-w-0 flex-1 select-text text-end text-xs text-foreground ${props.mono ? "font-mono" : ""}`}>{props.value}</Text>
    </View>
  );
}

export function CodeBlock(props: { label: string; value: string; empty?: string }) {
  const copied = signal(false);
  const copy = async () => {
    if (!props.value) return;
    try {
      await Clipboard.writeText(props.value);
      copied.set(true);
    } catch {
      copied.set(false);
    }
  };
  return (
    <View class="gap-1.5">
      <View class="flex-row items-center justify-between gap-2">
        <Text class="text-xs text-muted-foreground">{props.label}</Text>
        <If when={props.value}>
          <Button
            size="sm"
            label={copied() ? t("common.copied") : t("common.copy")}
            accessibilityLabel={`${t("common.copy")} ${props.label}`}
            onPress={() => void copy()}
          />
        </If>
      </View>
      <If when={props.value}>
        <Scroll class="max-h-80 rounded-(--radius) bg-code" contentClass="p-3">
          <Text class="select-text font-mono text-xs text-foreground">{props.value}</Text>
        </Scroll>
      </If>
      <If when={!props.value}>
        <Text class="text-sm text-muted-foreground">{props.empty ?? t("calls.none")}</Text>
      </If>
    </View>
  );
}

const CALL_STATUS_LABEL: Record<CallStatusView, string> = {
  running: "calls.status.running",
  completed: "calls.status.completed",
  error: "calls.status.error",
  cancelled: "calls.status.cancelled",
};

export function CallStatus(props: { status: CallStatusView }) {
  const tone = () =>
    props.status === "completed" ? "success" : props.status === "running" ? "neutral" : "warning";
  return <StatusBadge label={t(CALL_STATUS_LABEL[props.status])} tone={tone()} />;
}

export function RowButton(props: {
  selected?: boolean;
  disabled?: boolean;
  accessibilityLabel: string;
  onPress?: () => void;
  children?: JSX.Element;
}) {
  return (
    <BaseButton
      selected={props.selected}
      disabled={props.disabled}
      accessibilityLabel={props.accessibilityLabel}
      onPress={props.onPress}
      class={`min-h-11 w-full flex-row items-center gap-2 border-b border-border px-3 py-2 ${colorMotion} ${controlMotion} hover:bg-muted/70 active:bg-accent selected:bg-muted focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-45`}
    >
      {props.children}
    </BaseButton>
  );
}
