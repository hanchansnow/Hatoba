import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { setLocale, t } from "@/i18n";
import type { VaultStatus } from "@/ipc/types";

const mocks = vi.hoisted(() => {
  window.matchMedia = vi.fn().mockReturnValue({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() });
  return { password: vi.fn(), biometric: vi.fn(), enter: vi.fn(async () => {}) };
});
vi.mock("@/ipc/api", async (original) => ({ ...await original<typeof import("@/ipc/api")>(),
  api: { vault_unlock: mocks.password, vault_unlock_biometric: mocks.biometric } }));
vi.mock("@/app/boot", () => ({ enterUnlocked: mocks.enter }));
vi.mock("@/app/TitleBar", () => ({ WindowChrome: () => null }));

const { UnlockScreen } = await import("./UnlockScreen");
const { useApp } = await import("@/app/store");

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let unmount: (() => void) | null = null;
afterEach(() => {
  act(() => unmount?.());
  unmount = null;
  useApp.setState({ vault: null });
  vi.clearAllMocks();
});

function show() {
  setLocale("en");
  const host = document.createElement("div");
  document.body.appendChild(host);
  const root = createRoot(host);
  act(() => root.render(<UnlockScreen />));
  unmount = () => {
    root.unmount();
    host.remove();
  };
  const field = document.querySelector<HTMLInputElement>(`input[aria-label="${t("vault.unlock.field")}"]`)!;
  const reveal = () => document.querySelector<HTMLButtonElement>(`button[aria-label="${t("field.show")}"], button[aria-label="${t("field.hide")}"]`)!;
  return { host, field, reveal };
}

describe("unlock screen", () => {
  it("shows the password with its own button and keeps the focus in the field", () => {
    const { field, reveal } = show();
    act(() => field.focus());
    expect(field.type).toBe("password");
    expect(reveal().getAttribute("aria-label")).toBe(t("field.show"));

    const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true });
    reveal().dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    act(() => reveal().click());
    expect(field.type).toBe("text");
    expect(reveal().getAttribute("aria-label")).toBe(t("field.hide"));

    act(() => reveal().click());
    expect(field.type).toBe("password");
  });

  it("hides the password again when the window loses focus", () => {
    const { field, reveal } = show();
    act(() => reveal().click());
    expect(field.type).toBe("text");
    act(() => {
      window.dispatchEvent(new Event("blur"));
    });
    expect(field.type).toBe("password");
  });

  it("hides the password when a delay starts, since the field and its button are disabled", () => {
    const { field, reveal } = show();
    act(() => reveal().click());
    expect(field.type).toBe("text");
    act(() => useApp.setState({ vault: { retry_at: Date.now() + 30_000 } as VaultStatus }));
    expect(field.disabled).toBe(true);
    expect(reveal().disabled).toBe(true);
    expect(field.type).toBe("password");
  });
});

it.each(["password", "biometric"] as const)("shows %s unlock waiting without a false credential error", async (method) => {
  setLocale("en"); mocks.enter.mockClear();
  useApp.setState({ phase: "locked", info: { version: "test", platform: "macos", mica: false, local_user: "" },
    vault: { state: "locked", sync_kind: "none", biometric_enabled: true, retry_at: null } as VaultStatus });
  let finish!: () => void;
  mocks[method].mockImplementation(() => new Promise<void>((done) => { finish = done; }));
  const { host, field } = show();
  if (method === "password") {
    const input = field;
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
