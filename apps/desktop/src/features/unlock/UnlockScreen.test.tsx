import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { useApp } from "@/app/store";
import { setLocale } from "@/i18n";
import type { VaultStatus } from "@/ipc/types";
import { UnlockScreen } from "./UnlockScreen";

const mocks = vi.hoisted(() => {
  window.matchMedia = vi.fn().mockReturnValue({ matches: false, addEventListener: vi.fn() });
  return { password: vi.fn(), biometric: vi.fn(), enter: vi.fn(async () => {}) };
});
vi.mock("@/ipc/api", async (original) => ({ ...await original<typeof import("@/ipc/api")>(),
  api: { vault_unlock: mocks.password, vault_unlock_biometric: mocks.biometric } }));
vi.mock("@/app/boot", () => ({ enterUnlocked: mocks.enter }));
vi.mock("@/app/TitleBar", () => ({ WindowChrome: () => null }));
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
let cleanup: (() => void) | undefined;
afterEach(() => { act(() => cleanup?.()); });

it.each(["password", "biometric"] as const)("shows %s unlock waiting without a false credential error", async (method) => {
  setLocale("en"); mocks.enter.mockClear();
  useApp.setState({ phase: "locked", info: { version: "test", platform: "macos", mica: false, local_user: "" },
    vault: { state: "locked", sync_kind: "none", biometric_enabled: true, retry_at: null } as VaultStatus });
  let finish!: () => void;
  mocks[method].mockImplementation(() => new Promise<void>((done) => { finish = done; }));
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  cleanup = () => { root.unmount(); host.remove(); };
  act(() => root.render(<UnlockScreen />));
  if (method === "password") {
    const input = host.querySelector<HTMLInputElement>("input")!;
    act(() => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "password"); input.dispatchEvent(new Event("input", { bubbles: true })); });
  }
  const button = method === "password" ? host.querySelector<HTMLButtonElement>("button[type=submit]")!
    : [...host.querySelectorAll("button")].find((button) => button.textContent?.includes("Touch ID"))!;
  await act(async () => { button.click(); });
  expect(host.querySelector("[role=status]")?.textContent).toContain("Unlocking");
  expect(host.querySelector("[role=alert]")).toBeNull();
  expect(button.disabled).toBe(true);
  await act(async () => { finish(); });
  expect(mocks.enter).toHaveBeenCalledOnce();
});
