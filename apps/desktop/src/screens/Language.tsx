import { locale, Storage } from "nsh";
import { availableLocales, setLocale, t } from "nsh/i18n";
import type { SelectItem } from "nsh/base";

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

/** What a language picker offers here: the system, then every bundled language. */
export function languageItems(): SelectItem[] {
  return [
    { value: "system", title: t("language.system") },
    ...availableLocales().map((tag) => ({ value: tag, title: nameOf(tag) })),
  ];
}

/** The chosen tag, or `"system"` while the app follows the device. */
export function localePreference(): string {
  const shown = locale();
  return availableLocales().some((tag) => tag === shown) ? shown : "system";
}