import { beforeEach, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  state: "unlocked" as "unlocked" | "locked",
  phase: "locked",
  reload: vi.fn(async () => {}),
  clear: vi.fn(),
}));

vi.mock("@/ipc/api", () => ({ api: { sync_status: async () => ({}) } }));
vi.mock("./data", () => ({ useVaultData: { getState: () => backend } }));
vi.mock("./store", () => ({
  applyPrefs: vi.fn(), DEFAULT_PREFS: {},
  useApp: {
    getState: () => ({
      info: { platform: "linux" },
      refreshVault: async () => ({ state: backend.state }),
      setSync: vi.fn(),
    }),
    setState: (value: { phase: string }) => { backend.phase = value.phase; },
  },
}));

const { enterUnlocked } = await import("./boot");

beforeEach(() => {
  backend.state = "unlocked";
  backend.phase = "locked";
  backend.reload.mockReset().mockResolvedValue();
  backend.clear.mockClear();
});

it("keeps a restore that completed while locked on the lock screen", async () => {
  backend.state = "locked";
  await enterUnlocked();
  expect(backend.phase).toBe("locked");
  expect(backend.reload).not.toHaveBeenCalled();
  expect(backend.clear).toHaveBeenCalled();
});

it("does not reopen the main UI if locking happens during data reload", async () => {
  backend.reload.mockImplementation(async () => { backend.state = "locked"; });
  await enterUnlocked();
  expect(backend.phase).toBe("locked");
  expect(backend.clear).toHaveBeenCalled();
});

it("shows the main UI after an explicit unlock that remains unlocked", async () => {
  await enterUnlocked();
  expect(backend.phase).toBe("unlocked");
  expect(backend.clear).not.toHaveBeenCalled();
});


it("keeps the lock screen if locking makes data reload fail", async () => {
  backend.reload.mockImplementation(async () => {
    backend.state = "locked";
    throw new Error("vault locked during reload");
  });
  await enterUnlocked();
  expect(backend.phase).toBe("locked");
  expect(backend.clear).toHaveBeenCalled();
});
