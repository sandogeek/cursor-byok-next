import { Text, View } from "nsh";
import { Select, type SelectItem } from "nsh/base";
import { colorMotion } from "./motion";

/**
 * One value from a short list. The trigger is a field: it shares the input
 * border, height and inset so a form's text fields and selects line up.
 */
export function SelectField(props: {
  /** The visible label above the field; without one the row's title names it. */
  label?: string;
  accessibilityLabel: string;
  testID?: string;
  value: string;
  items: SelectItem[];
  disabled?: boolean;
  onChange: (value: string) => void;
}) {
  const title = () => props.items.find((item) => item.value === props.value)?.title ?? props.value;
  return (
    <View class="gap-1.5">
      <If when={props.label}>
        <Text class="text-xs text-muted-foreground">{props.label}</Text>
      </If>
      <Select
        items={props.items}
        value={props.value}
        disabled={props.disabled}
        accessibilityLabel={props.accessibilityLabel}
        testID={props.testID}
        onChange={({ value }) => props.onChange(value ?? "")}
        trigger={
          <View class={`min-h-8 flex-row items-center justify-between gap-2 rounded-(--radius) border border-input bg-background px-2.5 text-foreground ${colorMotion} hover:bg-muted disabled:opacity-45`}>
            <Text class="text-sm">{title()}</Text>
            <Text class="text-[10px] text-muted-foreground">▾</Text>
          </View>
        }
        popupClass="min-w-40 rounded-(--radius) border border-border bg-popover p-1 shadow-lg"
        itemClass="rounded-(--radius) px-2.5 py-1.5 text-sm text-popover-foreground focus:bg-muted selected:text-primary"
      />
    </View>
  );
}