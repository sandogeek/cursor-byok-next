import { Text, View, type JSX } from "nsh";

/**
 * One settings line: what it changes on the start, its explanation under it,
 * and the control at the end. The control owns its own height, so rows with a
 * switch, a button and a field still share one baseline. `separated={false}`
 * draws no divider under the last row of a group.
 */
export function SettingRow(props: { title: string; description?: string; separated?: boolean; children?: JSX.Element }) {
  return (
    <View class={`flex-row items-start justify-between gap-4 py-3 ${props.separated === false ? "" : "border-b border-border"}`}>
      <View class="min-w-0 flex-1 gap-1">
        <Text class="text-sm text-foreground">{props.title}</Text>
        <If when={props.description}>
          <Text class="text-xs text-muted-foreground">{props.description}</Text>
        </If>
      </View>
      <View class="shrink-0 flex-row items-center gap-2">{props.children}</View>
    </View>
  );
}