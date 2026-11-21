import { Text, type JSX } from "nsh";
import { t } from "nsh/i18n";
import { Button as BaseButton } from "nsh/base";
import { colorMotion, controlMotion } from "./motion";

export interface ButtonProps {
  children?: JSX.Element;
  label?: string;
  variant?: "primary" | "secondary" | "ghost";
  size?: "sm" | "md";
  disabled?: boolean;
  loading?: boolean;
  accessibilityLabel?: string;
  tooltip?: JSX.Element;
  testID?: string;
  onPress?: () => void;
}

export function Button(props: ButtonProps) {
  const variant = () => props.variant ?? "secondary";
  return (
    <BaseButton
      disabled={props.disabled || props.loading}
      accessibilityLabel={props.accessibilityLabel}
      tooltip={props.tooltip}
      testID={props.testID}
      onPress={props.onPress}
      class={`min-h-8 flex-row items-center justify-center gap-2 rounded-(--radius) px-3 text-sm ${controlMotion} active:scale-[0.97] focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-45 disabled:active:scale-100 ${variant() === "primary" ? "bg-primary text-primary-foreground hover:bg-primary-hover active:bg-primary-active" : variant() === "ghost" ? "bg-transparent text-muted-foreground hover:bg-muted hover:text-foreground active:bg-accent" : "border border-border bg-card text-card-foreground hover:bg-muted active:bg-secondary"}`}
    >
      {props.children ?? <Text class={props.size === "sm" ? "text-xs" : "text-sm"}>{props.loading ? t("common.loading") : props.label}</Text>}
    </BaseButton>
  );
}

export function IconButton(props: {
  label: string;
  icon: string;
  disabled?: boolean;
  loading?: boolean;
  onPress?: () => void;
}) {
  return (
    <BaseButton
      accessibilityLabel={props.label}
      tooltip={<Text class="text-xs text-popover-foreground">{props.label}</Text>}
      disabled={props.disabled || props.loading}
      onPress={props.onPress}
      class={`size-6 items-center justify-center rounded-(--radius) text-muted-foreground ${controlMotion} hover:bg-muted hover:text-foreground active:scale-95 active:bg-accent focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-45 disabled:active:scale-100`}
    >
      <Text class="text-base">{props.loading ? "⋯" : props.icon}</Text>
    </BaseButton>
  );
}
