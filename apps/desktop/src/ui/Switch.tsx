import { Switch as BaseSwitch } from "nsh/base";
import { colorMotion } from "./motion";

export function Switch(props: {
  checked: boolean;
  label: string;
  testID?: string;
  disabled?: boolean;
  onChange: (event: { checked: boolean }) => void;
}) {
  return (
    <BaseSwitch
      checked={props.checked}
      testID={props.testID}
      accessibilityLabel={props.label}
      disabled={props.disabled}
      onChange={props.onChange}
      class={`flex-row items-center rounded-(--radius) disabled:opacity-45 ${colorMotion} focus-visible:ring-2 focus-visible:ring-ring`}
      trackClass={`w-9 rounded-full bg-muted p-0.5 ${colorMotion} checked:bg-primary`}
      thumbClass={`size-4 rounded-full bg-card shadow-sm transition-transform duration-180 ease-in-out checked:translate-x-4 motion-reduce:transition-none`}
    />
  );
}
