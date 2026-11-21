import { createMotion, resource, Scroll, Text, View } from "nsh";
import { t } from "nsh/i18n";
import type { CallDetail as CallDetailRecord, CallRecord } from "cursor-byok-next/native";
import { Button } from "../ui/Button";
import { motion } from "../ui/motion";
import { CallStatus, CodeBlock, FieldRow } from "../ui/Record";
import { Surface } from "../ui/Surface";

export interface CallInspector {
  detail(callId: string): Promise<CallDetailRecord | null>;
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

function show(value: string | number | undefined | null) {
  if (value == null || value === "") return t("calls.none");
  return String(value);
}

function timing(value: number | undefined | null) {
  return value == null ? t("calls.none") : `${value} ms`;
}

function pretty(value: string) {
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

function yesNo(value: boolean) {
  return value ? t("common.yes") : t("common.no");
}

function CallFields(props: { call: CallRecord }) {
  const current = () => props.call;
  return (
    <Surface class="gap-3 p-4">
      <View class="flex-row items-center justify-between gap-3">
        <View class="gap-1">
          <Text role="header" class="text-base font-semibold text-foreground">{current().displayName}</Text>
          <Text class="select-text font-mono text-xs text-muted-foreground">{current().modelId}</Text>
        </View>
        <CallStatus status={current().status} />
      </View>
      <FieldRow label={t("calls.detail.fields.callId")} value={show(current().callId)} mono />
      <FieldRow label={t("calls.detail.fields.runId")} value={show(current().runId)} mono />
      <FieldRow label={t("calls.detail.fields.conversationId")} value={show(current().conversationId)} mono />
      <FieldRow label={t("calls.detail.fields.index")} value={show(current().providerCallIndex)} />
      <FieldRow label={t("calls.detail.fields.modelHash")} value={show(current().modelHash)} mono />
      <FieldRow label={t("calls.detail.fields.providerType")} value={show(current().providerType)} />
      <FieldRow label={t("calls.detail.fields.providerUrl")} value={show(current().providerUrl)} mono />
      <FieldRow label={t("calls.detail.fields.requestType")} value={show(current().requestType)} />
      <FieldRow label={t("calls.detail.fields.requestUrl")} value={show(current().requestUrl)} mono />
      <FieldRow label={t("calls.detail.fields.reasoning")} value={show(current().reasoningEffort)} />
      <FieldRow label={t("calls.detail.fields.fast")} value={yesNo(current().fast)} />
      <FieldRow label={t("calls.detail.fields.finish")} value={show(current().finishReason)} />
      <FieldRow label={t("calls.detail.fields.http")} value={show(current().httpStatus)} />
      <FieldRow label={t("calls.detail.fields.created")} value={`${current().createdAtMs} · ${new Date(current().createdAtMs).toLocaleString()}`} />
      <FieldRow label={t("calls.detail.fields.duration")} value={timing(current().durationMs)} />
      <FieldRow label={t("calls.detail.fields.responseHeaders")} value={timing(current().responseHeadersMs)} />
      <FieldRow label={t("calls.detail.fields.ttfr")} value={timing(current().ttfrMs)} />
      <FieldRow label={t("calls.detail.fields.ttft")} value={timing(current().ttftMs)} />
      <FieldRow label={t("calls.detail.fields.input")} value={show(current().inputTokens)} />
      <FieldRow label={t("calls.detail.fields.output")} value={show(current().outputTokens)} />
      <FieldRow label={t("calls.detail.fields.total")} value={show(current().totalTokens)} />
      <FieldRow label={t("calls.detail.fields.cacheRead")} value={show(current().cacheReadTokens)} />
      <FieldRow label={t("calls.detail.fields.cacheWrite")} value={show(current().cacheWriteTokens)} />
      <FieldRow label={t("calls.detail.fields.reasoningTokens")} value={show(current().reasoningTokens)} />
      <FieldRow label={t("calls.detail.fields.messages")} value={show(current().messageCount)} />
      <FieldRow label={t("calls.detail.fields.tools")} value={show(current().toolCount)} />
      <FieldRow label={t("calls.detail.fields.requestBytes")} value={show(current().requestBytes)} />
      <FieldRow label={t("calls.detail.fields.responseBytes")} value={show(current().responseBytes)} />
      <FieldRow label={t("calls.detail.fields.responseChunks")} value={show(current().responseChunkCount)} />
      <FieldRow label={t("calls.detail.fields.detailed")} value={yesNo(current().detailed)} />
      <FieldRow label={t("calls.detail.fields.errorKind")} value={show(current().errorKind)} />
      <FieldRow label={t("calls.detail.fields.errorMessage")} value={show(current().errorMessage)} />
    </Surface>
  );
}

export function CallDetails(props: { callId: string; inspector: CallInspector; onBack: () => void }) {
  const state = resource(() => props.inspector.detail(props.callId));
  const reveal = createMotion({
    enter: { keyframes: [{ opacity: 0, translateY: motion.distance }, { opacity: 1, translateY: 0 }], duration: motion.base, easing: "easeOut" },
  });
  const detail = () => state();
  const request = () => detail()?.request;
  const chunks = () => detail()?.chunks ?? [];
  const response = () => chunks().map((chunk) => chunk.data).join("");
  const responseBytes = () => chunks().reduce((total, chunk) => total + chunk.byteCount, 0);

  return (
    <View class="min-h-0 flex-1 gap-4">
      <View class="flex-row items-center gap-2">
        <Button label={t("common.back")} onPress={props.onBack} />
      </View>
      <If when={state.error()}>
        <Surface class="gap-2 p-4">
          <Text role="header" class="text-base font-semibold text-foreground">{t("calls.error.detail")}</Text>
          <Text class="text-sm text-muted-foreground">{errorText(state.error())}</Text>
          <Button label={t("common.retry")} variant="primary" loading={state.loading()} onPress={() => state.refetch()} />
        </Surface>
      </If>
      <If when={state.loading() && !detail()}>
        <Surface class="items-center gap-2 p-8">
          <Text class="text-sm text-muted-foreground">{t("calls.loading")}</Text>
        </Surface>
      </If>
      <If when={!state.error() && !state.loading() && !detail()}>
        <Surface class="items-center gap-2 p-8">
          <Text class="text-sm text-muted-foreground">{t("calls.detail.missing")}</Text>
        </Surface>
      </If>
      <If when={detail()}>
        {(current) => (
          <Scroll class="min-h-0 flex-1" contentClass="gap-4 pb-4">
            <View class="gap-4" style={reveal.style}>
              <CallFields call={current().call} />
              <Surface class="gap-3 p-4">
                <Text role="header" class="text-sm font-semibold text-foreground">{t("calls.detail.request")}</Text>
                <If when={request()}>
                  <Text class="text-xs text-muted-foreground">{t("calls.detail.bytes", { count: request()?.byteCount ?? 0 })}</Text>
                  <If when={current().call.requestTruncated}>
                    <Text role="alert" class="text-sm text-warning">{t("calls.detail.requestTruncated")}</Text>
                  </If>
                  <CodeBlock label={t("calls.detail.fields.headers")} value={pretty(request()?.headersJson ?? "")} />
                  <CodeBlock label={t("calls.detail.fields.body")} value={pretty(request()?.bodyJson ?? "")} />
                </If>
                <If when={!request()}>
                  <Text class="text-sm text-muted-foreground">{t("calls.detail.noRequest")}</Text>
                </If>
              </Surface>
              <Surface class="gap-3 p-4">
                <Text role="header" class="text-sm font-semibold text-foreground">{t("calls.detail.response")}</Text>
                <If when={current().call.responseTruncated}>
                  <Text role="alert" class="text-sm text-warning">{t("calls.detail.responseTruncated")}</Text>
                </If>
                <If when={chunks().length > 0}>
                  <Text class="text-xs text-muted-foreground">{t("calls.detail.chunks", { count: chunks().length, bytes: responseBytes() })}</Text>
                  <CodeBlock label={t("calls.detail.response")} value={response()} />
                </If>
                <If when={chunks().length === 0}>
                  <Text class="text-sm text-muted-foreground">{t("calls.detail.noResponse")}</Text>
                </If>
              </Surface>
            </View>
          </Scroll>
        )}
      </If>
    </View>
  );
}
