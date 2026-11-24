import { setColorScheme, Storage } from "nsh";

/** Where the app's own appearance choice lives; absent follows the system. */
export const APPEARANCE_KEY = "cursor-byok-next:appearance";

export type Appearance = "light" | "dark" | "system";

const APPEARANCES: Appearance[] = ["system", "light", "dark"];

const isAppearance = (value: string | null): value is Appearance =>
  value !== null && APPEARANCES.some((appearance) => appearance === value);

/** Applies the choice now; "system" follows the device again. */
export function chooseAppearance(appearance: Appearance) {
  setColorScheme(appearance);
  void (appearance === "system" ? Storage.removeItem(APPEARANCE_KEY) : Storage.setItem(APPEARANCE_KEY, appearance));
}

/** The stored choice; "system" while the user never made one. */
export async function readAppearance(): Promise<Appearance> {
  const saved = await Storage.getItem(APPEARANCE_KEY);
  return isAppearance(saved) ? saved : "system";
}

/**
 * The stored choice, applied at launch, before the first screen draws.
 * `null` while the app follows the system: `setColorScheme` is left as it is,
 * so the window chrome does not need a native call either.
 */
export async function restoreAppearance(): Promise<Appearance | null> {
  const saved = await readAppearance();
  if (saved === "system") return null;
  setColorScheme(saved);
  return saved;
}