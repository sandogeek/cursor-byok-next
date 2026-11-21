import { locale, localeSettings } from "nsh";

/** A string, or that string per language tag, as plugins and models write it. */
export type LocalizedText = string | { [locale: string]: string };

/**
 * The language the app shows, as `nsh/i18n` picks it: the first preferred
 * language the value has (itself or a shorter tag), else the device locale's,
 * else English, else any text there is.
 */
export function localized(text: LocalizedText | null | undefined): string {
  if (text === null || text === undefined) return "";
  if (typeof text === "string") return text;
  const tags = [...localeSettings().languages, locale()];
  for (const tag of tags) {
    let candidate: string | undefined = tag;
    while (candidate) {
      const found = text[candidate];
      if (found !== undefined && found.trim() !== "") return found;
      const cut = candidate.lastIndexOf("-");
      candidate = cut > 0 ? candidate.slice(0, cut) : undefined;
    }
  }
  return Object.values(text).find((value) => value.trim() !== "") ?? "";
}