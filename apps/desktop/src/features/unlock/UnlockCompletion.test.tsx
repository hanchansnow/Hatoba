import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ICloudBackupStatus, SyncStatus, VaultStatus } from "@/ipc/types";

const backend = vi.hoisted(() => ({ state: "locked" as "locked" | "unlocked", password: "current-password" }));
const mocks = vi.hoisted(() => {
  window.matchMedia = vi.fn().mockReturnValue({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() });
  return {
    password: vi.fn<(password: string) => Promise<void>>(),
    biometric: vi.fn<() => Promise<void>>(),
    recover: vi.fn<(code: string, password: string) => Promise<void>>(),
    reload: vi.fn<() => Promise<void>>(), clear: vi.fn(),
    sync: vi.fn<() => Promise<SyncStatus>>(), backup: vi.fn<() => Promise<ICloudBackupStatus>>(), toast: vi.fn(),
  };
});
vi.mock("@/ipc/api", async (original) => ({
  ...await original<typeof import("@/ipc/api")>(),
  api: {
    vault_unlock: mocks.password, vault_unlock_biometric: mocks.biometric, vault_recover: mocks.recover,
    vault_status: async () => ({ state: backend.state, sync_kind: "worker", biometric_enabled: true, retry_at: null } as VaultStatus),
    sync_status: mocks.sync, icloud_backup_status: mocks.backup,
  },
}));
vi.mock("@/app/data", () => ({ useVaultData: { getState: () => ({ reload: mocks.reload, clear: mocks.clear }) } }));
vi.mock("@/app/TitleBar", () => ({ WindowChrome: () => null }));
vi.mock("@/components/overlay", async (original) => ({ ...await original<typeof import("@/components/overlay")>(), toast: mocks.toast }));

const { useApp } = await import("@/app/store");
const { setLocale, t } = await import("@/i18n");
const { setOverlaysLocked } = await import("@/components/overlay");
const { UnlockScreen } = await import("./UnlockScreen");
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const sync = { kind: "worker", state: "idle", endpoint: "https://example.test", database: null, last_synced_at: null,
  pending: 0, conflicts: 0, auto_sync: true, message: null, counts: null, worker_update: null, too_large: [] } as SyncStatus;
const backup = { enabled: false, automatic: false, retention: 5, last_at: null, last_uploaded_at: null,
  error: null, entries: [], devices: [] } as ICloudBackupStatus;
let unmount: (() => void) | undefined;
beforeEach(() => {
  setLocale("en"); backend.state = "locked"; backend.password = "current-password";
  vi.clearAllMocks();
  mocks.password.mockImplementation(async (password) => {
    if (password !== backend.password) throw { code: "wrong_password", detail: "Wrong password" };
    backend.state = "unlocked";
  });
  mocks.biometric.mockImplementation(async () => { backend.state = "unlocked"; });
  mocks.recover.mockImplementation(async (_code, password) => { backend.password = password; backend.state = "unlocked"; });
  mocks.reload.mockReset().mockResolvedValue(); mocks.sync.mockReset().mockResolvedValue(sync);
  mocks.backup.mockReset().mockResolvedValue(backup);
  useApp.setState({ phase: "locked", vault: { state: "locked", sync_kind: "worker", biometric_enabled: true, retry_at: null } as VaultStatus,
    info: { ...useApp.getState().info, platform: "macos" } });
  setOverlaysLocked(true);
});
afterEach(() => {
  act(() => unmount?.()); unmount = undefined;
  setOverlaysLocked(false); useApp.setState({ phase: "boot", vault: null });
});
function Harness() {
  const phase = useApp((state) => state.phase);
  return phase === "locked" ? <UnlockScreen /> : <div>Main UI</div>;
}
function show() {
  const host = document.createElement("div"); document.body.append(host);
  const root = createRoot(host); act(() => root.render(<Harness />));
  unmount = () => { root.unmount(); host.remove(); };
  return host;
}
function input(field: HTMLInputElement, value: string) {
  act(() => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(field, value);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
}
function button(text: string) {
  return [...document.querySelectorAll<HTMLButtonElement>("button")].find((item) => item.textContent === text)!;
}
function deferred() {
  let finish!: () => void;
  return { wait: new Promise<void>((done) => { finish = done; }), finish: () => finish() };
}
async function startUnlock(host: HTMLElement, method: "password" | "biometric") {
  if (method === "password") input(host.querySelector("input")!, backend.password);
  const action = method === "password" ? host.querySelector<HTMLButtonElement>("button[type=submit]")! : button(t("vault.unlock.touchId"));
  await act(async () => { action.click(); });
}

it.each(["password", "biometric"] as const)("makes %s unlock usable after a relock during vault data loading", async (method) => {
  const pending = deferred(); mocks.reload.mockImplementation(() => pending.wait);
  const host = show(); await startUnlock(host, method);
  expect(mocks.reload).toHaveBeenCalledOnce(); expect(button(t("vault.unlock.touchId")).disabled).toBe(true);
  backend.state = "locked";
  await act(async () => { pending.finish(); });
  expect(useApp.getState().phase).toBe("locked"); expect(mocks.clear).toHaveBeenCalled();
  expect(host.querySelector<HTMLInputElement>("input")!.readOnly).toBe(false);
  expect(host.querySelector("[role=status]")).toBeNull(); expect(button(t("vault.unlock.touchId")).disabled).toBe(false);
  await startUnlock(host, method);
  expect(useApp.getState().phase).toBe("unlocked"); expect(host.textContent).toBe("Main UI");
});

it.each(["password", "biometric"] as const)("makes %s unlock usable after a relock during sync status loading", async (method) => {
  const pending = deferred(); mocks.sync.mockImplementation(async () => { await pending.wait; return sync; });
  const host = show(); await startUnlock(host, method); expect(mocks.sync).toHaveBeenCalledOnce();
  backend.state = "locked";
  await act(async () => { pending.finish(); });
  expect(useApp.getState().phase).toBe("locked"); expect(host.querySelector("[role=status]")).toBeNull();
  expect(button(t("vault.unlock.touchId")).disabled).toBe(false);
  await startUnlock(host, method); expect(useApp.getState().phase).toBe("unlocked");
});

it.each(["password", "biometric"] as const)("makes %s unlock usable after a relock during iCloud status loading", async (method) => {
  const pending = deferred(); mocks.backup.mockImplementation(async () => { await pending.wait; return backup; });
  const host = show(); await startUnlock(host, method); expect(mocks.backup).toHaveBeenCalledOnce();
  backend.state = "locked";
  await act(async () => { pending.finish(); });
  expect(useApp.getState().phase).toBe("locked"); expect(host.querySelector("[role=status]")).toBeNull();
  expect(button(t("vault.unlock.touchId")).disabled).toBe(false);
  await startUnlock(host, method); expect(useApp.getState().phase).toBe("unlocked");
});

function recovery() {
  const host = show(); act(() => button(t("vault.unlock.forgot")).click());
  input(document.querySelector("#recovery-code")!, "AAAA-BBBB-CCCC-DDDD");
  input(document.querySelector("#new-password")!, "new-master-password-123");
  input(document.querySelector("#confirm-password")!, "new-master-password-123");
  return host;
}
it("dismisses successful remote recovery that stays locked and allows unlock with the committed new password", async () => {
  const pending = deferred();
  mocks.recover.mockImplementation(async (_code, password) => { await pending.wait; backend.password = password; });
  const host = recovery(); await act(async () => button(t("vault.recover.submit")).click());
  expect(button(t("btn.cancel")).disabled).toBe(true);
  await act(async () => { pending.finish(); });
  expect(useApp.getState().phase).toBe("locked"); expect(document.querySelector("#recovery-code")).toBeNull();
  expect(mocks.toast).not.toHaveBeenCalled(); expect(backend.password).toBe("new-master-password-123");
  await startUnlock(host, "password");
  expect(mocks.password).toHaveBeenCalledWith("new-master-password-123"); expect(useApp.getState().phase).toBe("unlocked");
  expect(mocks.recover).toHaveBeenCalledOnce();
});
it("finishes committed recovery when later data loading fails, without presenting a recovery failure", async () => {
  mocks.reload.mockRejectedValue(new Error("Cannot load vault details"));
  recovery(); await act(async () => button(t("vault.recover.submit")).click());
  expect(backend.password).toBe("new-master-password-123"); expect(mocks.recover).toHaveBeenCalledOnce();
  expect(document.querySelector("#recovery-code")).toBeNull(); expect(mocks.toast).toHaveBeenCalledOnce();
  expect(mocks.toast.mock.calls[0]?.[1]).toBe("error");
});
it("keeps a failed recovery form retryable with its entered values and Cancel enabled", async () => {
  mocks.recover.mockRejectedValue({ code: "wrong_recovery_code", detail: "Wrong recovery code" });
  recovery(); await act(async () => button(t("vault.recover.submit")).click());
  expect(document.querySelector<HTMLInputElement>("#new-password")!.value).toBe("new-master-password-123");
  expect(button(t("btn.cancel")).disabled).toBe(false); expect(button(t("vault.recover.submit")).disabled).toBe(false);
  expect([...document.querySelectorAll("[role=alert]")].some((item) => !!item.textContent)).toBe(true);
  act(() => button(t("btn.cancel")).click()); expect(document.querySelector("#recovery-code")).toBeNull();
});
