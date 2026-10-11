import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useApp } from "@/app/store";
import { syncFooterView } from "@/app/Sidebar";
import { setLocale, t } from "@/i18n";
import type { ICloudBackupStatus, SyncStatus } from "@/ipc/types";
import { SyncPage } from "./SyncPage";
import { syncNeedsAttention } from "./status";

vi.hoisted(() => { window.matchMedia = vi.fn().mockReturnValue({ matches: false, addEventListener: vi.fn() }); });
vi.mock("./ICloudBackup", () => ({ ICloudBackup: () => <div>iCloud history</div> }));
vi.mock("./SyncStatusView", () => ({ SyncStatusView: ({ onBackup }: { onBackup?: () => void }) => <button onClick={onBackup}>Connection status</button> }));
vi.mock("./SyncConflictsView", () => ({ SyncConflictsView: () => <div>Conflicts</div> }));
vi.mock("./SyncWizard", () => ({ SyncWizard: () => <div>Setup</div> }));
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
const tr = Object.assign(t, { locale: "en" as const });
function sync(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return { kind: "worker", endpoint: "https://example.test", database: null, state: "idle", last_synced_at: 123,
    pending: 0, conflicts: 0, auto_sync: true, message: null, counts: null, worker_update: null, too_large: [], ...overrides };
}
function backup(transfer?: string): ICloudBackupStatus {
  return { enabled: true, automatic: true, retention: 5, last_at: transfer ? 123 : null,
    last_uploaded_at: transfer === "uploaded" ? 123 : null, error: null, devices: [],
    entries: transfer ? [{ name: "snapshot", created_at: 123, size: 1, transfer }] : [] };
}
let cleanup: (() => void) | undefined;
afterEach(() => { act(() => cleanup?.()); cleanup = undefined; });
function render() {
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  act(() => root.render(<SyncPage />)); cleanup = () => { root.unmount(); host.remove(); };
  return host;
}
describe("backup and sync status precedence", () => {
  it.each(["auth_failed", "offline", "error", "paused"] as const)("keeps %s visible with iCloud enabled", (state) => {
    setLocale("en"); const connection = sync({ state });
    expect(syncNeedsAttention(connection)).toBe(true);
    expect(syncFooterView(tr, connection, backup("uploaded"))).toEqual(syncFooterView(tr, connection));
    useApp.setState({ info: { ...useApp.getState().info, platform: "macos" }, sync: connection, icloudBackup: backup("uploaded") });
    const host = render(); expect(host.textContent).toBe("Connection status");
    act(() => host.querySelector("button")!.click()); expect(host.textContent).toBe("iCloud history");
  });
  it("keeps conflicts visible and opens the connection page by default", () => {
    const connection = sync({ conflicts: 2 });
    expect(syncFooterView(tr, connection, backup("uploaded")).color).toBe("var(--orange)");
    useApp.setState({ info: { ...useApp.getState().info, platform: "macos" }, sync: connection, icloudBackup: backup("uploaded") });
    expect(render().textContent).toBe("Connection status");
  });
  it("opens enabled iCloud automatically when no connection needs attention", () => {
    useApp.setState({ info: { ...useApp.getState().info, platform: "macos" }, sync: sync(), icloudBackup: backup("uploaded") });
    expect(render().textContent).toBe("iCloud history");
  });
  it("only confirmed uploads show green Synced; empty, saving, pending and failures remain distinct", () => {
    setLocale("en");
    expect(syncFooterView(tr, null, backup()).color).toBe("var(--fg2)");
    expect(syncFooterView(tr, null, backup("pending")).color).toBe("var(--accent)");
    expect(syncFooterView(tr, null, backup("uploading")).spin).toBe(true);
    expect(syncFooterView(tr, null, backup("uploaded")).color).toBe("var(--green)");
    expect(syncFooterView(tr, null, backup("timeout")).color).toBe("var(--red)");
    expect(syncFooterView(tr, null, backup("uploaded"), true).spin).toBe(true);
  });
});
