import { Clipboard, createMotion, resource, Scroll, signal, Text, View } from "nsh";
import { Harness, type HarnessState } from "cursor-byok/native";
import { Button, IconButton } from "../ui/Button";
import { Dialog } from "../ui/Dialog";
import { motion } from "../ui/motion";
import { NavigationGroup, NavigationItem, StatusBadge, Surface } from "../ui/Surface";
import { Switch } from "../ui/Switch";

export interface CursorTakeover {
  status(): Promise<HarnessState>;
  initializeCa(): Promise<HarnessState>;
  setEnabled(enabled: boolean): Promise<HarnessState>;
}

const initialState: HarnessState = {
  platform: "unknown",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
};

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

export function Cursor(props: { takeover: CursorTakeover }) {
  const state = resource(() => props.takeover.status(), { initialValue: initialState });
  const busy = signal(false);
  const error = signal("");
  const copied = signal(false);
  const confirmDisable = signal(false);
  const commandOpen = signal(false);
  const reveal = createMotion({
    enter: { keyframes: [{ opacity: 0, translateY: motion.distance }, { opacity: 1, translateY: 0 }], duration: motion.base, easing: "easeOut" },
  });
  const current = () => state() ?? initialState;
  const enabled = () => current().integration === "enabled";
  const statusLabel = () => current().integration === "degraded" ? "状态异常" : enabled() ? "已接管" : "未接管";
  const takeoverLabel = () => enabled() ? "关闭接管 Cursor" : "开启接管 Cursor";
  const caReady = () => current().ca === "ready";
  const statusTone = () => current().integration === "degraded" ? "warning" : enabled() ? "success" : "neutral";

  const perform = async (operation: () => Promise<HarnessState>) => {
    if (busy()) return;
    busy.set(true);
    error.set("");
    copied.set(false);
    try {
      state.mutate(await operation());
    } catch (cause) {
      error.set(errorText(cause));
      state.refetch();
    } finally {
      busy.set(false);
    }
  };
  const refresh = () => {
    error.set("");
    copied.set(false);
    state.refetch();
  };
  const initialize = () => perform(async () => {
    const next = await props.takeover.initializeCa();
    if (next.ca === "untrusted" && next.caInstallCommand) commandOpen.set(true);
    return next;
  });
  const changeTakeover = (checked: boolean) => {
    if (checked) void perform(() => props.takeover.setEnabled(true));
    else confirmDisable.set(true);
  };
  const copyCommand = async () => {
    const command = current().caInstallCommand;
    if (!command) return;
    try {
      await Clipboard.writeText(command);
      copied.set(true);
      error.set("");
    } catch (cause) {
      error.set(`复制失败：${errorText(cause)}`);
    }
  };

  return (
    <View class="flex-1 flex-row bg-background p-4 pt-[env(safe-area-inset-top)]">
      <Surface class="w-60 shrink-0 self-stretch p-1.5">
        <View class="gap-0.5">
          <NavigationItem label="数据概览" icon="▦" disabled />
          <NavigationItem label="调用详细" icon="≡" disabled />
          <NavigationGroup label="模型配置" />
          <NavigationItem
            label="Cursor"
            icon="⌁"
            selected
            badge={<StatusBadge label={statusLabel()} tone={statusTone()} />}
          />
          <NavigationGroup label="设置" />
          <NavigationItem label="插件配置" icon="◇" disabled />
          <NavigationItem label="系统设置" icon="⊙" disabled />
          <NavigationItem label="使用教程" icon="?" disabled />
        </View>
        <View class="flex-1" />
        <View class="gap-1 border-t border-sidebar-border px-2.5 pb-1 pt-3">
          <Text class="text-xs font-medium text-sidebar-foreground">Cursor BYOK</Text>
          <Text class="text-xs text-muted-foreground">本地接管 · NSH</Text>
        </View>
      </Surface>

      <View class="min-w-0 flex-1 pl-4">
        <View class="min-h-16 flex-row items-center justify-between gap-4 pb-3 pl-1" desktop={{ windowDrag: true }}>
          <Text role="header" class="text-2xl font-semibold text-foreground">Cursor</Text>
          <Surface class="flex-row items-center gap-2 p-1.5">
            <Text class="pl-1 text-xs text-muted-foreground">{statusLabel()}</Text>
            <Switch
              checked={enabled()}
              label={takeoverLabel()}
              disabled={busy() || state.loading() || (!enabled() && !caReady())}
              onChange={({ checked }) => changeTakeover(checked)}
            />
            <IconButton label="刷新" icon="↻" loading={state.loading()} disabled={busy()} onPress={refresh} />
          </Surface>
        </View>

        <Scroll class="min-h-0 flex-1" contentClass="grow">
          <View class="min-h-64 flex-1 items-center justify-center gap-3 rounded-(--radius-lg) border border-dashed border-border p-8" style={reveal.style}>
            <If when={state.error()}>
              <Text role="header" class="text-base font-semibold text-foreground">暂时无法读取接管状态</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">{errorText(state.error())}</Text>
              <Button label="重试" variant="primary" loading={state.loading()} onPress={refresh} />
            </If>
            <If when={!state.error() && current().ca === "missing"}>
              <Text role="header" class="text-base font-semibold text-foreground">需要先初始化本地 CA</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">CA 仅保存在本机，用于安全解析 Cursor 的 HTTPS 请求。</Text>
              <Button label="初始化 CA" variant="primary" loading={busy()} onPress={() => void initialize()} />
            </If>
            <If when={!state.error() && current().ca === "untrusted"}>
              <Text role="header" class="text-base font-semibold text-foreground">需要在系统中信任本地 CA</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">复制授权命令，在终端中执行并输入密码，完成后刷新状态。</Text>
              <View class="flex-row gap-2">
                <Button label="安装本地 CA…" variant="primary" disabled={!current().caInstallCommand} onPress={() => commandOpen.set(true)} />
                <Button label="刷新状态" loading={state.loading()} disabled={busy()} onPress={refresh} />
              </View>
            </If>
            <If when={!state.error() && current().ca === "invalid"}>
              <Text role="header" class="text-base font-semibold text-foreground">本地 CA 无效</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">证书或密钥不完整、不匹配或已过期。重新初始化前请检查本地 CA 文件。</Text>
              <Button label="重新检查" variant="primary" loading={state.loading()} onPress={refresh} />
            </If>
            <If when={!state.error() && caReady()}>
              <Text class="text-3xl text-success">✓</Text>
              <Text role="header" class="text-base font-semibold text-foreground">{enabled() ? "Cursor 接管已生效" : "本地 CA 已就绪"}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">
                {current().integration === "degraded" ? "本地代理与 Cursor 配置不一致，请刷新状态或重新开启接管。" : enabled() ? "Cursor 正在通过本地代理连接。模型配置将在后续接入。" : "开启右上角接管开关后，Cursor 才会使用本地代理。"}
              </Text>
              <If when={current().proxyUrl}>
                <View class="rounded-(--radius) bg-code px-3 py-2">
                  <Text class="select-text font-mono text-xs text-muted-foreground">{current().proxyUrl}</Text>
                </View>
              </If>
              <If when={!enabled()}>
                <Button label="开启接管 Cursor" variant="primary" loading={busy()} onPress={() => void perform(() => props.takeover.setEnabled(true))} />
              </If>
            </If>
            <If when={error()}>
              <Text role="alert" class="max-w-md text-center text-sm text-warning">{error()}</Text>
            </If>
          </View>
        </Scroll>
      </View>

      <Dialog
        open={confirmDisable()}
        title="关闭接管 Cursor？"
        description="关闭后将移除本应用写入的 Cursor 本地代理配置，其他代理设置不受影响。"
        cancelLabel="取消"
        confirmLabel="关闭接管"
        onCancel={() => confirmDisable.set(false)}
        onConfirm={() => {
          confirmDisable.set(false);
          void perform(() => props.takeover.setEnabled(false));
        }}
      />
      <Dialog
        open={commandOpen()}
        title="安装本地 CA"
        description="证书授权需要管理员密码。应用只复制命令，不会替你执行。"
        cancelLabel="关闭"
        confirmLabel="复制命令"
        onCancel={() => commandOpen.set(false)}
        onConfirm={() => void copyCommand()}
      >
        <View class="gap-3">
          <Text class="text-sm text-popover-foreground">将下方命令粘贴到终端执行，然后回到此页刷新状态。</Text>
          <View class="rounded-(--radius) bg-code p-3">
            <Text class="select-text font-mono text-xs text-popover-foreground">{current().caInstallCommand}</Text>
          </View>
          <If when={copied()}>
            <Text class="text-sm text-success">命令已复制</Text>
          </If>
        </View>
      </Dialog>
    </View>
  );
}
