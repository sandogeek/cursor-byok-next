import { Text, View } from "nsh";
import { Input } from "nsh/base";
import { colorMotion } from "./motion";

export function Field(props: {
  label: string;
  value: string;
  placeholder?: string;
  masked?: boolean;
  keyboardType?: "default" | "url";
  invalid?: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <View class="gap-1.5">
      <Text class="text-xs text-muted-foreground">{props.label}</Text>
      <Input
        value={props.value}
        placeholder={props.placeholder}
        masked={props.masked}
        keyboardType={props.keyboardType}
        autoCorrect={false}
        autoCapitalize="none"
        accessibilityLabel={props.label}
        onChange={({ value }) => props.onChange(value)}
        class={`min-h-8 rounded-(--radius) border bg-background px-2.5 text-sm text-foreground ${colorMotion} ${props.invalid ? "border-destructive" : "border-input"} focus:border-ring focus-visible:ring-2 focus-visible:ring-ring`}
      />
    </View>
  );
}
