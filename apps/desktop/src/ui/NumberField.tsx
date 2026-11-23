import { Text, View } from "nsh";
import { Input } from "nsh/base";
import { colorMotion } from "./motion";

/**
 * A whole-number field. The text is the app's: a caller decides what counts as
 * a valid number, so a half-typed value stays visible instead of snapping.
 */
export function NumberField(props: {
  /** The visible label above the field; without one the row's title names it. */
  label?: string;
  accessibilityLabel: string;
  testID?: string;
  value: string;
  placeholder?: string;
  disabled?: boolean;
  invalid?: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <View class="gap-1.5">
      <If when={props.label}>
        <Text class="text-xs text-muted-foreground">{props.label}</Text>
      </If>
      <Input
        value={props.value}
        placeholder={props.placeholder}
        disabled={props.disabled}
        accessibilityLabel={props.accessibilityLabel}
        testID={props.testID}
        keyboardType="numbers-and-punctuation"
        autoCorrect={false}
        autoCapitalize="none"
        onChange={({ value }) => props.onChange(value)}
        class={`min-h-8 rounded-(--radius) border bg-background px-2.5 font-mono text-sm tabular-nums text-foreground ${colorMotion} ${props.invalid ? "border-destructive" : "border-input"} focus:border-ring focus-visible:ring-2 focus-visible:ring-ring`}
      />
    </View>
  );
}