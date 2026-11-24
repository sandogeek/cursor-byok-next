import { appInfo, Clipboard, openUrl, Scroll, createMotion, resource, signal, Text, View } from "nsh";
import { t } from "nsh/i18n";
import { shellInfo } from "nsh/updater";
import type { CallsSnapshot, HarnessState } from "cursor-byok-next/native";
import type { CursorTakeover, DesktopPreferences } from "./Cursor";
import { chooseAppearance, readAppearance, type Appearance } from "../ui/appearance";
import { Button } from "../ui/Button";
import { ChoiceGroup } from "../ui/ChoiceGroup";
import { Dialog } from "../ui/Dialog";
import { NumberField } from "../ui/NumberField";
import { SelectField } from "../ui/Select";
import { SettingRow } from "../ui/SettingRow";
import { StatusBadge, Surface } from "../ui/Surface";
import { Switch } from "../ui/Switch";
import { motion } from "../ui/motion";
import { chooseLocale, languageItems, localePreference } from "./Language";
import { checkForUpdates, installUpdate, installedVersion, updateState } from "./updates";

/** What the settings page needs of the call log: counts, mode and clearing. */
export interface SettingsCallLog {
  snapshot(): Promise<CallsSnapshot>;
  setDetailed(enabled: boolean): Promise<CallsSnapshot>;
  clearDetails(): Promise<CallsSnapshot>;
  clearAll(): Promise<CallsSnapshot>;
}

const initialTracking: HarnessState = {
  platform: "unknown",
  ca: "missing",
  integration: "disabled",
  settingsApplied: false,
  preferredProxyPort: null,
  dataDir: "",
};

const emptyCalls: CallsSnapshot = {
  calls: [],
  detailed: false,
  storage: { callCount: 0, requestCount: 0, chunkCount: 0 },
  degraded: false,
};

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

/** Ports are 0–65535, an empty field clears the choice, and `0` asks the OS. */
function parsePort(text: string): number | null | undefined {
  const trimmed = text.trim();
  if (trimmed === "") return null;
  if (!/^\d{1,5}$/.test(trimmed)) return undefined;
  const port = Number(trimmed);
  return port <= 65_535 ? port : undefined;
}

export function SystemSettings(props: { takeover: CursorTakeover; calls: SettingsCallLog; desktop: DesktopPreferences }) {
  /** The takeover state holds the port the user chose and the app's data dir. */
  const tracking = resource(() => props.takeover.status(), { initialValue: initialTracking });
  const calls = resource(() => props.calls.snapshot(), { initialValue: emptyCalls });
  const desktop = resource(() => props.desktop.snapshot(), { initialValue: { showDockIcon: true } });
  const appearance = signal<Appearance>("system");
  const busy = signal(false);
  const error = signal("");
  const notice = signal("");
  const portDraft = signal<string | null>(null);
  const confirmClear = signal<"details" | "all" | null>(null);
  const reveal = createMotion({
    enter: { keyframes: [{ opacity: 0, translateY: motion.distance }, { opacity: 1, translateY: 0 }], duration: motion.base, easing: "easeOut" },
  });

  const current = () => tracking() ?? initialTracking;
  const log = () => calls() ?? emptyCalls;
  /** The edit, or the saved port while the user has not typed. */
  const savedPortText = () => current().preferredProxyPort == null ? "" : String(current().preferredProxyPort);
  const portText = () => portDraft() ?? savedPortText();
  const portEdited = () => portDraft() !== null && portDraft() !== savedPortText();
  const portInvalid = () => portEdited() && parsePort(portText()) === undefined;
  const version = () => shownVersion();

  /** The appearance the app starts with was restored in `App.tsx`; mirror it. */
  void readAppearance().then(appearance.set);

  const perform = async (operation: () => Promise<void>) => {
    if (busy()) return;
    busy.set(true);
    error.set("");
    notice.set("");
    try {
      await operation();
    } catch (cause) {
      error.set(errorText(cause));
    } finally {
      busy.set(false);
    }
  };

  const savePort = () => {
    const port = parsePort(portText());
    if (port === undefined) return;
    void perform(async () => {
      tracking.mutate(await props.takeover.setPreferredPort(port));
      portDraft.set(null);
      notice.set(t("settings.port.saved"));
    });
  };

  const toggleDetailed = (enabled: boolean) => {
    void perform(async () => {
      calls.mutate(await props.calls.setDetailed(enabled));
    });
  };

  const toggleDock = (visible: boolean) => {
    void perform(async () => {
      desktop.mutate(await props.desktop.setDockIconVisible(visible));
    });
  };

  const clear = (scope: "details" | "all") => {
    confirmClear.set(null);
    void perform(async () => {
      calls.mutate(scope === "all" ? await props.calls.clearAll() : await props.calls.clearDetails());
      notice.set(scope === "all" ? t("settings.storage.clearedAll") : t("settings.storage.clearedDetails"));
    });
  };

  const copyDataDir = async () => {
    try {
      await Clipboard.writeText(current().dataDir);
      notice.set(t("settings.about.copied"));
    } catch (cause) {
      error.set(errorText(cause));
    }
  };

  const openReleasePage = async () => {
    const update = updateState().update;
    if (!update) return;
    try {
      await openUrl(update.url);
    } catch (cause) {
      error.set(errorText(cause));
    }
  };

  /** The offered version, or the installed one when nothing newer is known. */
  const shownVersion = () => shellInfo().version ?? appInfo().version ?? "";
  const updateBusy = () => updateState().status === "checking" || updateState().status === "downloading";

  const appearanceOptions = () => [
    { value: "system", title: t("settings.appearance.system") },
    { value: "light", title: t("settings.appearance.light") },
    { value: "dark", title: t("settings.appearance.dark") },
  ];

  return (
    <Scroll class="min-h-0 flex-1" contentClass="gap-4 pb-4">
      <View class="gap-4" style={reveal.style}>
        <Surface class="gap-1 p-4">
          <SettingRow title={t("settings.port.title")} description={t("settings.port.description")}>
            <View class="w-28">
              <NumberField
                accessibilityLabel={t("settings.port.title")}
                testID="settings-port"
                value={portText()}
                placeholder="0"
                invalid={portInvalid()}
                disabled={busy()}
                onChange={(value) => portDraft.set(value)}
              />
            </View>
            <Button
              label={t("settings.port.save")}
              variant="primary"
              size="sm"
              loading={busy()}
              disabled={!portEdited() || portInvalid()}
              onPress={savePort}
            />
          </SettingRow>
          <View class="flex-row items-center justify-between gap-3 pt-3">
            <Text class="text-xs text-muted-foreground">
              {t("settings.port.current", { port: current().proxyPort ? String(current().proxyPort) : t("settings.port.unbound") })}
            </Text>
            <If when={portInvalid()}>
              <Text role="alert" class="text-xs text-destructive">{t("settings.port.invalid")}</Text>
            </If>
          </View>
        </Surface>

        <Surface class="gap-3 p-4">
          <View class="flex-row items-center justify-between gap-3">
            <Text role="header" class="text-sm font-semibold text-foreground">{t("settings.calls.title")}</Text>
            <Text class="text-xs text-muted-foreground">
              {t("settings.calls.count", {
                calls: log().storage.callCount,
                requests: log().storage.requestCount,
                chunks: log().storage.chunkCount,
              })}
            </Text>
          </View>
          <SettingRow title={t("settings.calls.detailed")} description={t("settings.calls.detailedHint")}>
            <Switch
              checked={log().detailed}
              label={t("settings.calls.detailed")}
              disabled={busy() || calls.loading()}
              onChange={({ checked }) => toggleDetailed(checked)}
            />
          </SettingRow>
          <View class="flex-row items-center justify-between gap-3 pt-1">
            <Text class="text-xs text-muted-foreground">{t("settings.calls.retention")}</Text>
            <View class="flex-row gap-2">
              <Button
                label={t("settings.calls.clearDetails")}
                disabled={busy() || calls.loading() || (log().storage.requestCount === 0 && log().storage.chunkCount === 0)}
                onPress={() => confirmClear.set("details")}
              />
              <Button
                label={t("settings.calls.clearAll")}
                disabled={busy() || calls.loading() || log().storage.callCount === 0}
                onPress={() => confirmClear.set("all")}
              />
            </View>
          </View>
        </Surface>

        <Surface class="gap-1 p-4">
          <If when={current().platform === "macos"}>
            <SettingRow title={t("settings.dock.title")} description={t("settings.dock.description")}>
              <Switch
                checked={desktop().showDockIcon}
                testID="settings-dock"
                label={t("settings.dock.title")}
                disabled={busy() || desktop.loading()}
                onChange={({ checked }) => toggleDock(checked)}
              />
            </SettingRow>
          </If>
          <SettingRow title={t("settings.appearance.title")} description={t("settings.appearance.description")}>
            <ChoiceGroup
              label={t("settings.appearance.title")}
              value={appearance()}
              options={appearanceOptions()}
              onChange={(next) => {
                const value = next as Appearance;
                appearance.set(value);
                chooseAppearance(value);
              }}
            />
          </SettingRow>
          <SettingRow
            title={t("settings.language.title")}
            description={t("settings.language.description")}
            separated={false}
          >
            <View class="w-40">
              <SelectField
                accessibilityLabel={t("settings.language.field")}
                testID="settings-language"
                value={localePreference()}
                items={languageItems()}
                onChange={(value) => chooseLocale(value === "system" ? null : value)}
              />
            </View>
          </SettingRow>
        </Surface>

        <Surface class="gap-3 p-4">
          <View class="flex-row items-center justify-between gap-3">
            <Text role="header" class="text-sm font-semibold text-foreground">{t("settings.updates.title")}</Text>
            <StatusBadge
              label={updateState().update ? updateState().update!.version : t("settings.updates.check")}
              tone={updateState().status === "failed" ? "warning" : updateState().status === "available" ? "success" : "neutral"}
            />
          </View>
          <SettingRow title={t("settings.about.version", { version: version() })} description={t("settings.updates.description")}>
            <Button
              label={updateBusy() ? t("settings.updates.checking") : t("settings.updates.check")}
              testID="settings-check-update"
              loading={updateBusy()}
              disabled={updateBusy()}
              onPress={() => void checkForUpdates()}
            />
          </SettingRow>
          <If when={updateState().status === "idle"}>
            <Text class="text-xs text-muted-foreground">{t("settings.updates.upToDate")}</Text>
          </If>
          <If when={updateState().update !== null && (updateState().status === "available" || updateState().status === "downloading" || updateState().status === "ready")}>
            <View class="gap-2 rounded-(--radius) border border-border bg-muted px-3 py-2">
              <Text class="text-sm text-foreground">
                {t("settings.updates.available", { version: updateState().update?.version ?? "" })}
              </Text>
              <If when={updateState().update?.shell}>
                <Text class="text-xs text-muted-foreground">{t("settings.updates.shell")}</Text>
              </If>
              <If when={updateState().update?.notes}>
                <View class="gap-1">
                  <Text class="text-xs text-muted-foreground">{t("settings.updates.notes")}</Text>
                  <Text class="select-text text-xs text-foreground">{updateState().update?.notes ?? ""}</Text>
                </View>
              </If>
              <If when={updateState().status === "downloading"}>
                <Text class="text-xs text-muted-foreground">
                  {t("settings.updates.downloading", {
                    received: String(updateState().received),
                    total: updateState().total == null ? "?" : String(updateState().total),
                  })}
                </Text>
              </If>
              <If when={updateState().status === "ready"}>
                <Text class="text-xs text-success">{t("settings.updates.ready")}</Text>
              </If>
              <View class="flex-row items-center gap-2 pt-1">
                <Button
                  label={t("settings.updates.install")}
                  variant="primary"
                  size="sm"
                  testID="settings-install-update"
                  disabled={!updateState().update?.canInstall || updateBusy() || updateState().status === "ready"}
                  onPress={() => void installUpdate()}
                />
                <If when={!updateState().update?.canInstall}>
                  <Button
                    label={t("settings.updates.open")}
                    size="sm"
                    onPress={() => void openReleasePage()}
                  />
                </If>
              </View>
              <If when={!updateState().update?.canInstall}>
                <Text class="text-xs text-muted-foreground">{t("settings.updates.unavailable")}</Text>
              </If>
            </View>
          </If>
          <If when={updateState().status === "failed"}>
            <Text role="alert" class="text-xs text-destructive">
              {t("settings.updates.failed", { message: updateState().error })}
            </Text>
          </If>
        </Surface>

        <Surface class="gap-3 p-4">
          <View class="flex-row items-center justify-between gap-3">
            <Text role="header" class="text-sm font-semibold text-foreground">{t("settings.about.title")}</Text>
            <StatusBadge label={current().platform} tone="neutral" />
          </View>
          <View class="gap-1 rounded-(--radius) bg-code px-3 py-2">
            <Text class="text-xs text-muted-foreground">{t("settings.about.dataDir")}</Text>
            <Text class="select-text font-mono text-xs text-foreground">{current().dataDir || t("calls.none")}</Text>
          </View>
          <View class="flex-row items-center justify-between gap-3">
            <Text class="text-xs text-muted-foreground">{t("settings.about.version", { version: version() })}</Text>
            <Button label={t("settings.about.copy")} disabled={!current().dataDir} onPress={() => void copyDataDir()} />
          </View>
          <Text class="text-xs text-muted-foreground">{t("settings.about.body")}</Text>
        </Surface>

        <If when={error()}>
          <Text role="alert" class="px-1 text-sm text-warning">{error()}</Text>
        </If>
        <If when={notice()}>
          <Text class="px-1 text-sm text-muted-foreground">{notice()}</Text>
        </If>
      </View>

      <Dialog
        open={confirmClear() === "details"}
        title={t("settings.dialog.details.title")}
        description={t("settings.dialog.details.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("settings.dialog.details.confirm")}
        onCancel={() => confirmClear.set(null)}
        onConfirm={() => clear("details")}
      />
      <Dialog
        open={confirmClear() === "all"}
        title={t("settings.dialog.all.title")}
        description={t("settings.dialog.all.body")}
        cancelLabel={t("common.cancel")}
        confirmLabel={t("settings.dialog.all.confirm")}
        onCancel={() => confirmClear.set(null)}
        onConfirm={() => clear("all")}
      />
    </Scroll>
  );
}