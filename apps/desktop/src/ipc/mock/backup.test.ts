import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "./index";

beforeEach(() => { vi.useFakeTimers(); history.replaceState(null, "", "/?platform=macos"); });
afterEach(() => { vi.useRealTimers(); history.replaceState(null, "", "/"); });

async function snapshot(api: ReturnType<typeof createMockApi>) {
  const result = api.icloud_backup_now();
  await vi.advanceTimersByTimeAsync(300);
  return result;
}

it("normal mock uploads finish and stay complete, without a false timeout", async () => {
  const api = createMockApi();
  await api.icloud_backup_configure(true, false);
  expect((await snapshot(api)).entries[0].transfer).toBe("pending");
  for (let n = 0; n < 10; n++) expect((await api.icloud_backup_status()).entries[0].transfer).toBe("pending");
  await vi.advanceTimersByTimeAsync(2000);
  expect((await api.icloud_backup_status()).entries[0].transfer).toBe("uploading");
  await vi.advanceTimersByTimeAsync(2000);
  expect((await api.icloud_backup_status()).entries[0].transfer).toBe("uploaded");
  await vi.advanceTimersByTimeAsync(20000);
  expect((await api.icloud_backup_status()).last_uploaded_at).not.toBeNull();
  expect((await api.icloud_backup_status()).entries[0].transfer).toBe("uploaded");
});

it.each(["error", "timeout"])("an explicit %s demo stops after five checks and can retry", async (state) => {
  history.replaceState(null, "", `/?platform=macos&backup=${state}`);
  const api = createMockApi();
  await api.icloud_backup_configure(true, false);
  await snapshot(api);
  for (let n = 0; n < 4; n++) { await vi.advanceTimersByTimeAsync(2000); await api.icloud_backup_status(); }
  expect((await api.icloud_backup_status()).entries[0].transfer).toBe(state);
  await vi.advanceTimersByTimeAsync(20000);
  expect((await api.icloud_backup_status()).entries[0].transfer).toBe(state);
  expect((await api.icloud_backup_status(true)).entries[0].transfer).not.toBe(state);
});

it("retention one keeps the uploaded copy until its replacement finishes", async () => {
  const api = createMockApi();
  await api.icloud_backup_configure(true, false, 1);
  const old = (await snapshot(api)).entries[0].name;
  for (let n = 0; n < 2; n++) { await vi.advanceTimersByTimeAsync(2000); await api.icloud_backup_status(); }
  expect((await snapshot(api)).entries).toHaveLength(2);
  await vi.advanceTimersByTimeAsync(2000);
  expect((await api.icloud_backup_status()).entries.some((entry) => entry.name === old)).toBe(true);
  await vi.advanceTimersByTimeAsync(2000);
  const done = await api.icloud_backup_status();
  expect(done.entries).toHaveLength(1);
  expect(done.entries[0].name).not.toBe(old);
  expect(done.entries[0].transfer).toBe("uploaded");
});
