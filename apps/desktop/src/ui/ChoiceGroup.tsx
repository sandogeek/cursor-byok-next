import { Text, View } from "nsh";
import { Radio, RadioGroup } from "nsh/base";
import { colorMotion } from "./motion";

/**
 * A segmented choice: one control, one silhouette, and one selection fill.
 * For a short set whose options stay visible.
 */
export function ChoiceGroup(props: {
  label: string;
  value: string;
  options: { value: string; title: string }[];
  disabled?: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <RadioGroup accessibilityLabel={props.label} class="flex-row rounded-(--radius) border border-input bg-muted p-0.5">
      <Each of={props.options} key={(option) => option.value}>
        {(option) => {
          const selected = () => props.value === option().value;
          return (
            <Radio
              checked={selected()}
              disabled={props.disabled}
              accessibilityLabel={option().title}
              onChange={() => props.onChange(option().value)}
              class={`min-h-7 flex-row items-center justify-center rounded-(--radius) px-2.5 ${colorMotion} ${selected() ? "bg-card shadow-sm" : "hover:bg-card/60"} disabled:opacity-45`}
            >
              <Text class={`text-xs ${selected() ? "font-medium text-foreground" : "text-muted-foreground"}`}>{option().title}</Text>
            </Radio>
          );
        }}
      </Each>
    </RadioGroup>
  );
}