import { act, useState } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { useApp } from "@/app/store";
import { setOverlaysLocked } from "@/components/overlay";
import { setLocale } from "@/i18n";
import { BackupRestore, ICloudBackup } from "./ICloudBackup";
import { RestoreNotice } from "./RestoreNotice";

const mocks = vi.hoisted(() => {
  window.matchMedia = vi.fn().mockReturnValue({ matches: false, addEventListener: vi.fn() });
  return { restore: vi.fn(), status: vi.fn(), listen: vi.fn(async () => () => {}) };
});
vi.mock("@/ipc/api", async (original) => ({ ...await original<typeof import("@/ipc/api")>(),
  api: { icloud_backup_restore: mocks.restore, icloud_backup_status: mocks.status, listen: mocks.listen }, isTauri: () => false }));
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
let cleanup: (() => void) | undefined;
afterEach(() => { act(() => cleanup?.()); setOverlaysLocked(false); useApp.setState({ backupRestore: null }); });
function Harness() {
  const phase = useApp((state) => state.phase);
  const [closed, close] = useState(false);
  return <>{phase === "unlocked" && !closed && <BackupRestore path="test" onClose={() => close(true)} />}<RestoreNotice /></>;
}
it.each(["io", "sqlite"])("keeps %s restore failure visible after locking unmounts the form", async (code) => {
  setLocale("en"); useApp.setState({ phase: "unlocked", backupRestore: null });
  let reject!: (error: unknown) => void;
  mocks.restore.mockImplementation(() => new Promise((_, fail) => { reject = fail; }));
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  cleanup = () => { root.unmount(); host.remove(); };
  act(() => root.render(<Harness />));
  const input = document.querySelector<HTMLInputElement>("input[type=password]")!;
  act(() => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "backup-password"); input.dispatchEvent(new Event("input", { bubbles: true })); });
  const restore = [...document.querySelectorAll("button")].find((button) => button.textContent === "Restore")!;
  await act(async () => { restore.click(); await Promise.resolve(); });
  expect(useApp.getState().backupRestore?.running).toBe(true);
  act(() => { setOverlaysLocked(true); useApp.setState({ phase: "locked" }); });
  expect(document.querySelector("input[type=password]")).toBeNull();
  expect(document.querySelector("[role=status]")?.textContent).toContain("restoring");
  await act(async () => { reject({ code, detail: "injected failure" }); await Promise.resolve(); });
  expect(useApp.getState().backupRestore?.running).toBe(false);
  expect(document.querySelector("[role=alert]")?.textContent).toBeTruthy();
  act(() => document.querySelector<HTMLButtonElement>("button")!.click());
  expect(useApp.getState().backupRestore).toBeNull();
});

it("shows histories from every device with the existing current-device badge and restores the selected device file", async () => {
  setLocale("en"); useApp.setState({ phase: "unlocked", backupRestore: null });
  vi.spyOn(document, "hasFocus").mockReturnValue(true);
  const entries = (name: string) => [{ name, created_at: 1_700_000_000_000, size: 42, transfer: "uploaded" }];
  const devices = [
    { device_id: "device-a", name: "Mac A", platform: "macOS", current: true, last_at: 1_700_000_000_000, last_uploaded_at: 1_700_000_000_000, entries: entries("snapshot-a"), error: null },
    { device_id: "device-b", name: "Mac B", platform: "macOS", current: false, last_at: 1_700_000_000_000, last_uploaded_at: 1_700_000_000_000, entries: entries("snapshot-b"), error: null },
  ];
  mocks.status.mockResolvedValue({ enabled: true, automatic: true, retention: 5, last_at: 1_700_000_000_000, last_uploaded_at: 1_700_000_000_000, entries: devices[0].entries, devices, error: null });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  cleanup = () => { root.unmount(); host.remove(); vi.restoreAllMocks(); };
  await act(async () => { root.render(<ICloudBackup />); await Promise.resolve(); });
  expect(host.textContent).toContain("Mac A"); expect(host.textContent).toContain("Mac B");
  expect(host.textContent).toContain("This PC");
  const restores = [...host.querySelectorAll("button")].filter((button) => button.textContent === "Restore");
  expect(restores).toHaveLength(2);
  act(() => restores[1].click());
  expect(document.querySelector("[role=alertdialog]")?.textContent).toContain("device-b/snapshot-b");
});
