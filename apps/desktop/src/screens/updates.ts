import { appInfo, signal } from "nsh";
import {
  checkForUpdate,
  downloadUpdate,
  installOnQuit,
  shellInfo,
  type AppUpdate,
} from "nsh/updater";

/**
 * The update check state, owned here and mirrored into a signal: the
 * settings page reads the signal, and the check runs once per launch even
 * when the page is not open.
 */
export type UpdateStatus = "idle" | "checking" | "available" | "downloading" | "ready" | "failed";

export interface UpdateState {
  status: UpdateStatus;
  /** The offered version; `null` when the app is current or cannot tell. */
  update: AppUpdate | null;
  /** Bytes received of the archive, for the progress line. */
  received: number;
  /** The archive's size, when the manifest says it. */
  total: number | null;
  error: string;
}

const initial: UpdateState = {
  status: "idle",
  update: null,
  received: 0,
  total: null,
  error: "",
};

/** The one update state the app shows. */
export const updateState = signal<UpdateState>(initial);

/** The installed app's version: the one the release notes and the update check use. */
export function installedVersion() {
  return shellInfo().version ?? appInfo().version ?? "";
}

function describe(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * Asks the signed manifest for a newer release. `nsh/updater` resolves
 * `null` in a development run and on the web, where nothing updates.
 */
export async function checkForUpdates() {
  if (updateState().status === "checking" || updateState().status === "downloading") return;
  const current = updateState();
  updateState.set({ ...current, status: "checking", error: "" });
  try {
    const update = await checkForUpdate();
    updateState.set({
      ...updateState(),
      status: update ? "available" : "idle",
      update,
      error: "",
    });
  } catch (cause) {
    updateState.set({ ...updateState(), status: "failed", error: describe(cause) });
  }
}

/** Downloads the offered release and stages it to install when the app quits. */
export async function installUpdate() {
  const update = updateState().update;
  if (!update?.canInstall) return;
  if (updateState().status === "downloading") return;
  updateState.set({ ...updateState(), status: "downloading", received: 0, total: null, error: "" });
  try {
    await downloadUpdate(({ received, total }) => {
      updateState.set({ ...updateState(), received, total: total ?? null });
    });
    await installOnQuit();
    updateState.set({ ...updateState(), status: "ready" });
  } catch (cause) {
    updateState.set({ ...updateState(), status: "failed", error: describe(cause) });
  }
}