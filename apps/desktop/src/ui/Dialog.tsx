import { Text, View, type JSX } from "nsh";
import { Dialog as BaseDialog } from "nsh/base";
import { Button } from "./Button";

export function Dialog(props: {
  open: boolean;
  title: string;
  description?: string;
  children?: JSX.Element;
  cancelLabel?: string;
  confirmLabel: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <BaseDialog
      open={props.open}
      role="alertdialog"
      animation="fade"
      accessibilityLabel={props.title}
      accessibilityDescription={props.description}
      onOpenChange={({ open }) => { if (!open) props.onCancel(); }}
      backdropClass="bg-scrim"
      class="m-auto w-96 max-w-[90%] gap-4 rounded-(--radius-lg) border border-border bg-popover p-5 shadow-lg"
    >
      <View class="gap-2">
        <Text role="header" class="text-lg font-semibold text-popover-foreground">{props.title}</Text>
        <If when={props.description}>
          <Text class="text-sm text-muted-foreground">{props.description}</Text>
        </If>
      </View>
      {props.children}
      <View class="flex-row justify-end gap-2">
        <Button label={props.cancelLabel ?? "取消"} onPress={props.onCancel} />
        <Button label={props.confirmLabel} variant="primary" onPress={props.onConfirm} />
      </View>
    </BaseDialog>
  );
}
