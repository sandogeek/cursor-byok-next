import { Clipboard, createMotion, effect, platform, resource, Scroll, signal, Text, View } from "nsh";
import { Tray, currentWindow, titleBar } from "nsh/desktop";
import { t } from "nsh/i18n";
import type { HarnessState, DesktopState } from "cursor-byok-next/native";
import type { ImageAsset } from "nsh";
import type { WindowRef } from "nsh/desktop";
import { Button, IconButton } from "../ui/Button";
import { Dialog } from "../ui/Dialog";
import { motion } from "../ui/motion";
import { NavigationGroup, NavigationItem, StatusBadge, Surface } from "../ui/Surface";
import { Switch } from "../ui/Switch";
import type { Appearance } from "../ui/appearance";
import { CallDetails } from "./CallDetails";
import { Calls, type CallLog } from "./Calls";
import { Plugins, type PluginCatalog } from "./Plugins";
import { SystemSettings } from "./SystemSettings";

export interface CursorTakeover {
  status(): Promise<HarnessState>;
  initializeCa(): Promise<HarnessState>;
  setEnabled(enabled: boolean): Promise<HarnessState>;
  setPreferredPort(port: number | null): Promise<HarnessState>;
}

const initialState: HarnessState = {
  platform: "unknown",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
  preferredProxyPort: null,
  dataDir: "",
};

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

export interface DesktopPreferences {
  snapshot(): Promise<DesktopState>;
  restore(): Promise<void>;
  setDockIconVisible(visible: boolean): Promise<DesktopState>;
  setWindowPresence(presence: "open" | "closed"): Promise<void>;
  setAppearance(appearance: Appearance): Promise<void>;
}

export function Cursor(props: { takeover: CursorTakeover; plugins: PluginCatalog; calls: CallLog; desktop: DesktopPreferences; trayIcon: ImageAsset | string; onWindow?: (window: WindowRef) => void }) {
  const window = currentWindow();
  props.onWindow?.(window);
  const syncWindowPresence = (presence: "open" | "closed") =>
    props.desktop.setWindowPresence(presence).catch((cause: unknown) => {
      console.warn("the Dock icon state did not follow the window", cause);
    });
  const reopen = () => {
    if (platform.os === "macos") {
      // Restore Regular activation policy before focusing when Dock is enabled
      // so the menu bar and Dock icon appear with the window.
      void syncWindowPresence("open").finally(() => window.focus());
      return;
    }
    window.focus();
  };
  Tray.set({
    icon: props.trayIcon,
    tooltip: "Cursor-BYOK-Next",
    onClick: reopen,
    menu: [
      { label: t("tray.open"), onSelect: reopen },
      { type: "separator" },
      { role: "quit" },
    ],
  });
  if (platform.os === "macos") {
    window.set({ closeOnPress: false });
    window.on("closePress", () => window.hide());
    let wasOpen: boolean | undefined;
    effect(() => {
      const state = window.state();
      if (!state) return;
      const open = state.visible || state.minimized;
      if (wasOpen !== undefined && open !== wasOpen) {
        void syncWindowPresence(open ? "open" : "closed");
      }
      wasOpen = open;
    });
  }
  const page = signal<"cursor" | "plugins" | "calls" | "settings">("cursor");
  const selectedCall = signal<string | null>(null);
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
  const statusLabel = () => current().integration === "degraded" ? t("takeover.status.degraded") : enabled() ? t("takeover.status.enabled") : t("takeover.status.disabled");
  const takeoverLabel = () => enabled() ? t("takeover.switch.disable") : t("takeover.switch.enable");
  const caReady = () => current().ca === "ready";
  const statusTone = () => current().integration === "degraded" ? "warning" : enabled() ? "success" : "neutral";
  const title = () =>
    page() === "plugins" ? t("nav.plugins")
      : page() === "calls" ? t("nav.calls")
        : page() === "settings" ? t("nav.systemSettings")
          : t("nav.cursor");

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
      error.set(t("takeover.error.copy", { message: errorText(cause) }));
    }
  };

  return (
    <View class="flex-1 flex-row bg-background">
      <Surface variant="sidebar" class="w-60 shrink-0 self-stretch pt-[env(safe-area-inset-top)]">
        {/* The system title bar is hidden: macOS's traffic lights float over the
            sidebar's top left, so the strip keeps their height free and drags
            the window as the bar it replaced did. Windows' and GTK's window
            buttons are on the right instead, where the header reserves them. */}
        <If when={platform.os === "macos"}>
          <View class="shrink-0" style={{ height: titleBar().height }} desktop={{ windowDrag: true }} />
        </If>
        <View class="min-h-0 flex-1 p-1.5">
          <View class="gap-0.5">
            <NavigationItem label={t("nav.overview")} icon="chart" disabled />
            <NavigationItem
              label={t("nav.calls")}
              icon="list"
              selected={page() === "calls"}
              onPress={() => {
                selectedCall.set(null);
                page.set("calls");
              }}
            />
            <NavigationGroup label={t("nav.modelConfig")} />
            <NavigationItem
              label={t("nav.cursor")}
              icon="cursor"
              selected={page() === "cursor"}
              badge={<StatusBadge label={statusLabel()} tone={statusTone()} />}
              onPress={() => page.set("cursor")}
            />
            <NavigationGroup label={t("nav.settings")} />
            <NavigationItem
              label={t("nav.plugins")}
              icon="plugin"
              selected={page() === "plugins"}
              onPress={() => page.set("plugins")}
            />
            <NavigationItem
              label={t("nav.systemSettings")}
              icon="settings"
              selected={page() === "settings"}
              onPress={() => page.set("settings")}
            />
          </View>
        </View>
      </Surface>

      <View class="min-w-0 flex-1 px-4 pb-4 pt-[env(safe-area-inset-top)]">
        <View class="min-h-16 flex-row items-center justify-between gap-4 pb-3 pl-1" style={{ paddingRight: titleBar().right }} desktop={{ windowDrag: true }}>
          <Text role="header" class="text-2xl font-semibold text-foreground">{title()}</Text>
          <If when={page() === "cursor"}>
          <Surface class="flex-row items-center gap-2 p-1.5">
            <Text class="pl-1 text-xs text-muted-foreground">{statusLabel()}</Text>
            <Switch
              checked={enabled()}
              label={takeoverLabel()}
              disabled={busy() || state.loading() || (!enabled() && !caReady())}
              onChange={({ checked }) => changeTakeover(checked)}
            />
            <IconButton label={t("common.refresh")} icon="↻" loading={state.loading()} disabled={busy()} onPress={refresh} />
          </Surface>
          </If>
        </View>

        <If when={page() === "plugins"}>
          <Plugins catalog={props.plugins} />
        </If>
        <If when={page() === "settings"}>
          <SystemSettings takeover={props.takeover} calls={props.calls} desktop={props.desktop} />
        </If>
        <If when={page() === "calls" && selectedCall() === null}>
          <Calls log={props.calls} onOpen={(callId) => selectedCall.set(callId)} />
        </If>
        <If when={page() === "calls" && selectedCall() !== null}>
          <CallDetails
            callId={selectedCall() ?? ""}
            inspector={props.calls}
            onBack={() => selectedCall.set(null)}
          />
        </If>
        <If when={page() === "cursor"}>
        <Scroll class="min-h-0 flex-1" contentClass="grow">
          <View class="min-h-64 flex-1 items-center justify-center gap-3 rounded-(--radius-lg) border border-dashed border-border p-8" style={reveal.style}>
            <If when={state.error()}>
              <Text role="header" class="text-base font-semibold text-foreground">{t("takeover.error.status")}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">{errorText(state.error())}</Text>
              <Button label={t("common.retry")} variant="primary" loading={state.loading()} onPress={refresh} />
            </If>
            <If when={!state.error() && current().ca === "missing"}>
              <Text role="header" class="text-base font-semibold text-foreground">{t("takeover.ca.missing.title")}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">{t("takeover.ca.missing.body")}</Text>
              <Button label={t("takeover.ca.initialize")} variant="primary" loading={busy()} onPress={() => void initialize()} />
            </If>
            <If when={!state.error() && current().ca === "untrusted"}>
              <Text role="header" class="text-base font-semibold text-foreground">{t("takeover.ca.untrusted.title")}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">{t("takeover.ca.untrusted.body")}</Text>
              <View class="flex-row gap-2">
                <Button label={t("takeover.ca.install")} variant="primary" disabled={!current().caInstallCommand} onPress={() => commandOpen.set(true)} />
                <Button label={t("takeover.ca.refresh")} loading={state.loading()} disabled={busy()} onPress={refresh} />
              </View>
            </If>
            <If when={!state.error() && current().ca === "invalid"}>
              <Text role="header" class="text-base font-semibold text-foreground">{t("takeover.ca.invalid.title")}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">{t("takeover.ca.invalid.body")}</Text>
              <Button label={t("takeover.ca.recheck")} variant="primary" loading={state.loading()} onPress={refresh} />
            </If>
            <If when={!state.error() && caReady()}>
              <Text class="text-3xl text-success">✓</Text>
              <Text role="header" class="text-base font-semibold text-foreground">{enabled() ? t("takeover.ready.enabled") : t("takeover.ready.disabled")}</Text>
              <Text class="max-w-md text-center text-sm text-muted-foreground">
                {current().integration === "degraded" ? t("takeover.ready.body.degraded") : enabled() ? t("takeover.ready.body.enabled") : t("takeover.ready.body.disabled")}
              </Text>
              <If when={current().proxyUrl}>
                <View class="rounded-(--radius) bg-code px-3 py-2">
                  <Text class="select-text font-mono text-xs text-muted-foreground">{current().proxyUrl}</Text>
                </View>
              </If>
              <If when={!enabled()}>
                <Button label={t("takeover.switch.enable")} variant="primary" loading={busy()} onPress={() => void perform(() => props.takeover.setEnabled(true))} />
              </If>
            </If>
            <If when={error()}>
              <Text role="alert" class="max-w-md text-center text-sm text-warning">{error()}</Text>
            </If>
          </View>
        </Scroll>
        </If>
      </View>

      <Dialog
        open={confirmDisable()}
        title={t("takeover.dialog.disable.title")}
        description={t("takeover.dialog.disable.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("takeover.dialog.disable.confirm")}
        onCancel={() => confirmDisable.set(false)}
        onConfirm={() => {
          confirmDisable.set(false);
          void perform(() => props.takeover.setEnabled(false));
        }}
      />
      <Dialog
        open={commandOpen()}
        title={t("takeover.dialog.ca.title")}
        description={t("takeover.dialog.ca.body")}
        cancelLabel={t("common.close")}
        confirmLabel={t("takeover.dialog.ca.confirm")}
        onCancel={() => commandOpen.set(false)}
        onConfirm={() => void copyCommand()}
      >
        <View class="gap-3">
          <Text class="text-sm text-popover-foreground">{t("takeover.dialog.ca.hint")}</Text>
          <View class="rounded-(--radius) bg-code p-3">
            <Text class="select-text font-mono text-xs text-popover-foreground">{current().caInstallCommand}</Text>
          </View>
          <If when={copied()}>
            <Text class="text-sm text-success">{t("takeover.dialog.ca.copied")}</Text>
          </If>
        </View>
      </Dialog>
    </View>
  );
}