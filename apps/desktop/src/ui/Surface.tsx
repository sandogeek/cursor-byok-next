import { Text, token, View, type JSX } from "nsh";
import { Button } from "nsh/base";
import { Icon, type IconName } from "./Icon";
import { colorMotion } from "./motion";

export function Surface(props: { children?: JSX.Element; class?: string; variant?: "card" | "sidebar" }) {
  return (
    <View class={`${props.variant === "sidebar" ? "border-r border-sidebar-border bg-sidebar" : "rounded-(--radius-lg) border border-border bg-card"} ${props.class ?? ""}`}>
      {props.children}
    </View>
  );
}

export function StatusBadge(props: { label: string; tone?: "neutral" | "success" | "warning" }) {
  return (
    <View class={`rounded-full border px-1.5 py-0.5 ${props.tone === "success" ? "border-success/30 bg-success/10" : props.tone === "warning" ? "border-warning/30 bg-warning/10" : "border-border bg-muted"}`}>
      <Text class={`text-xs ${props.tone === "success" ? "text-success" : props.tone === "warning" ? "text-warning" : "text-muted-foreground"}`}>{props.label}</Text>
    </View>
  );
}

export function NavigationItem(props: {
  label: string;
  icon: IconName;
  selected?: boolean;
  disabled?: boolean;
  badge?: JSX.Element;
  onPress?: () => void;
}) {
  return (
    <Button
      selected={props.selected}
      disabled={props.disabled}
      accessibilityLabel={props.label}
      onPress={props.onPress}
      class={`min-h-9 flex-row items-center gap-2 rounded-(--radius) px-2.5 text-sidebar-foreground ${colorMotion} hover:bg-sidebar-accent/60 active:bg-sidebar-accent selected:bg-sidebar-accent selected:text-sidebar-accent-foreground focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-40`}
    >
      <View class="h-5 w-5 items-center justify-center">
        <Icon name={props.icon} color={props.selected ? token("sidebar-accent-foreground") : token("sidebar-foreground")} />
      </View>
      <Text class="flex-1 text-sm">{props.label}</Text>
      {props.badge}
    </Button>
  );
}

export function NavigationGroup(props: { label: string }) {
  return <Text class="px-2.5 pb-1 pt-3 text-xs text-muted-foreground">{props.label}</Text>;
}
