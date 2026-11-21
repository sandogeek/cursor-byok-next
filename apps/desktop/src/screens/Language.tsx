import { locale, Storage, Text, View } from "nsh";
import { availableLocales, setLocale, t } from "nsh/i18n";
import { Select } from "nsh/base";

/** Where an explicit language choice lives; absent means follow the system. */
export const LOCALE_KEY = "cursor-byok-next:locale";

/**
 * Each language's own name, as the system's language pickers write it. A tag
 * without a name here shows itself.
 */
const NAMES: Record<string, string> = {
  "en-US": "English",
  "zh-CN": "中文",
};

/** Stores the choice (`null` follows the system again) and applies it now. */
export function chooseLocale(tag: string | null) {
  setLocale(tag);
  void (tag === null ? Storage.removeItem(LOCALE_KEY) : Storage.setItem(LOCALE_KEY, tag));
}

/** Applies a saved choice at launch, before the first screen draws. */
export async function restoreLocale() {
  const saved = await Storage.getItem(LOCALE_KEY);
  if (saved) setLocale(saved);
}

const nameOf = (tag: string) => NAMES[tag] ?? tag;

/**
 * The app's translations, and "follow the system" for the device's language.
 * The app shows the user's own language until a choice is made here.
 */
export function LanguagePicker() {
  const items = () => [
    { value: "system", title: t("language.system") },
    ...availableLocales().map((tag) => ({ value: tag, title: nameOf(tag) })),
  ];
  const chosen = () => {
    const shown = locale();
    return availableLocales().some((tag) => tag === shown) ? shown : "system";
  };
  const shownName = () => chosen() === "system" ? t("language.system") : nameOf(chosen());
  return (
    <View class="gap-1">
      <Text class="px-0.5 text-xs text-muted-foreground">{t("language.label")}</Text>
      <Select
        items={items()}
        value={chosen()}
        accessibilityLabel={t("language.label")}
        onChange={({ value }) => chooseLocale(value === "system" || value === null ? null : value)}
        trigger={
          <View class="min-h-8 flex-row items-center justify-between gap-2 rounded-(--radius) border border-sidebar-border bg-card px-2.5 text-sidebar-foreground hover:bg-muted">
            <Text class="text-xs">{shownName()}</Text>
            <Text class="text-[10px] text-muted-foreground">▾</Text>
          </View>
        }
        popupClass="rounded-(--radius) border border-border bg-popover p-1 shadow-lg"
        itemClass="rounded-(--radius) px-2 py-1.5 text-xs text-popover-foreground focus:bg-muted selected:text-primary"
      />
    </View>
  );
}