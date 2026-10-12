/** Restore metadata only; passwords and vault contents are never persisted here. */
export interface RestoreOperation {
  running: boolean;
  error: string | null;
  completed?: boolean;
  safetyBackup?: string | null;
  failedItems?: string[];
}

const KEY = "hatoba.restoreResult";

export function readRestoreResult(): RestoreOperation | null {
  try {
    const result = JSON.parse(sessionStorage.getItem(KEY) ?? "null");
    if (!result || result.running !== false || !(result.completed === true || typeof result.error === "string")) return null;
    return { running: false, completed: result.completed === true, error: typeof result.error === "string" ? result.error : null,
      safetyBackup: typeof result.safetyBackup === "string" ? result.safetyBackup : null,
      failedItems: Array.isArray(result.failedItems) ? result.failedItems.filter((id: unknown) => typeof id === "string" && /^[A-Za-z0-9_-]{1,64}$/.test(id)) : [] };
  } catch { return null; }
}

export function writeRestoreResult(result: RestoreOperation | null): void {
  try {
    if (!result || result.running) sessionStorage.removeItem(KEY);
    else sessionStorage.setItem(KEY, JSON.stringify(result));
  } catch { /* The live notice still works if session storage is unavailable. */ }
}
