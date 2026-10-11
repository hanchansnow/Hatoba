import type { SyncStatus } from "@/ipc/types";

/** A connection's problems remain visible when another backup method is enabled. */
export function syncNeedsAttention(status: SyncStatus | null): boolean {
  return !!status && status.kind !== "none" && (["auth_failed", "offline", "error", "paused"].includes(status.state)
    || status.conflicts > 0 || status.too_large.length > 0);
}
