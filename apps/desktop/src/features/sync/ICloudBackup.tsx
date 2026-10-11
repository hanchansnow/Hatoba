import { useCallback, useEffect, useRef, useState } from "react";
import { useApp } from "@/app/store";
import { enterUnlocked } from "@/app/boot";
import { errorMessage } from "@/app/errors";
import { Button, Icon, Spinner, Switch, TextField } from "@/components/controls";
import { Group, Row, Section } from "@/components/layout";
import { Dialog, toast } from "@/components/overlay";
import { useT, translate, formatBytes, formatDateTime } from "@/i18n";
import { api, isTauri } from "@/ipc/api";
import type { ICloudBackupStatus } from "@/ipc/types";
import s from "./ICloudBackup.module.css";
import page from "./Sync.module.css";

export async function chooseBackupFile(): Promise<string | null> {
  if (!isTauri()) return "/Users/demo/Library/Mobile Documents/com~apple~CloudDocs/Hatoba/demo.hatoba-backup";
  const { open } = await import("@tauri-apps/plugin-dialog");
  const result = await open({ multiple: false, filters: [{ name: "Hatoba Backup", extensions: ["hatoba-backup", "hatoba"] }] });
  return typeof result === "string" ? result : null;
}

export function ICloudBackup({ onBack }: { onBack?: () => void }) {
  const t = useT();
  const locale = t.locale;
  const [status, setStatus] = useState<ICloudBackupStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [checked, setChecked] = useState(0);
  const [active, setActive] = useState(() => !document.hidden && document.hasFocus());
  const failures = useRef(0);
  const mounted = useRef(false);
  const request = useRef(0);
  const inFlight = useRef(0);
  const pending = useRef(false);
  const [restore, setRestore] = useState<string | null>(null);
  const showStatus = useCallback((next: ICloudBackupStatus) => {
    failures.current = next.error ? failures.current + 1 : 0;
    setStatus(next);
    useApp.getState().setICloudBackup(next);
    setRefreshError(failures.current >= 10 ? translate(locale, "backup.checkFailed") : null);
  }, [locale]);
  const load = useCallback(async (retry = false) => {
    const id = ++request.current;
    if (retry) failures.current = 0;
    inFlight.current++;
    setRefreshing(true);
    try {
      const next = await api.icloud_backup_status(retry);
      if (mounted.current && id === request.current) showStatus(next);
    } catch (e) {
      if (mounted.current && id === request.current) {
        failures.current++;
        setRefreshError(failures.current >= 10 ? translate(locale, "backup.checkFailed")
          : errorMessage((key, params) => translate(locale, key, params), e));
      }
    } finally {
      inFlight.current--;
      if (mounted.current) {
        if (id === request.current) setRefreshing(false);
        setChecked((value) => value + 1);
      }
    }
  }, [locale, showStatus]);
  useEffect(() => {
    mounted.current = true;
    let foreground = !document.hidden && document.hasFocus();
    setActive(foreground);
    if (foreground) void load();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    // Automatic backup results notify the open page without an idle polling loop.
    void api.listen("backup://changed", () => {
      if (!foreground) return;
      pending.current = true;
      setChecked((value) => value + 1);
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => {});
    const update = () => {
      const next = !document.hidden && document.hasFocus();
      if (next === foreground) return;
      foreground = next;
      setActive(next);
      if (next) { if (inFlight.current === 0) void load(); else pending.current = true; }
    };
    document.addEventListener("visibilitychange", update);
    window.addEventListener("focus", update);
    window.addEventListener("blur", update);
    return () => {
      mounted.current = false;
      disposed = true; unlisten?.();
      document.removeEventListener("visibilitychange", update);
      window.removeEventListener("focus", update);
      window.removeEventListener("blur", update);
    };
  }, [load]);
  useEffect(() => {
    const waiting = status?.entries.some((entry) => !["uploaded", "error", "timeout"].includes(entry.transfer));
    if (!active || busy || refreshing || inFlight.current > 0) return;
    if (pending.current) { pending.current = false; void load(); return; }
    if (failures.current >= 10 || (!waiting && !refreshError && !status?.error)) return;
    // The backend caps each file at ten checks; terminal entries never trigger another timer.
    const timer = window.setTimeout(() => {
      if (!document.hidden && document.hasFocus() && inFlight.current === 0) void load();
    }, 2_000);
    return () => window.clearTimeout(timer);
  }, [active, busy, refreshing, checked, status, refreshError, load]);
  const perform = async (action: () => Promise<ICloudBackupStatus | void>, backup = false) => {
    // Ignore an older query that finishes while a new backup is being saved.
    request.current++; setRefreshing(false);
    setBusy(true); setSaving(backup); setError(null); setRefreshError(null);
    setStatus((previous) => previous ? { ...previous, error: null } : previous);
    try {
      const next = await action();
      if (!mounted.current) return;
      if (next) { showStatus(next); setChecked((value) => value + 1); }
      else await load();
    }
    catch (e) { setError(errorMessage(t, e)); }
    finally { if (mounted.current) { setBusy(false); setSaving(false); } }
  };
  return <div className={`${page.page} ${active ? "" : s.paused}`}><div className={page.scroll}><div className={page.column}>
    {onBack && <div className={s.actions}><Button icon="arrow-left" disabled={busy} onClick={onBack}>{t("btn.back")}</Button></div>}
    <div><div className={page.title}>{t("backup.title")}</div><p className={s.hint}>{t("backup.description")}</p></div>
    <Group>
      <Row label={t("backup.enabled")}><Switch label={t("backup.enabled")} checked={status?.enabled ?? false}
        disabled={busy || !status}
        onChange={(enabled) => void perform(() => api.icloud_backup_configure(enabled, status?.automatic ?? false))} /></Row>
      <Row label={t("backup.auto")}><Switch label={t("backup.auto")} checked={status?.automatic ?? false}
        disabled={busy || !status?.enabled}
        onChange={(automatic) => void perform(() => api.icloud_backup_configure(status?.enabled ?? false, automatic))} /></Row>
    </Group>
    <div className={s.actions}>
      <Button variant="primary" icon="cloud-arrow-up" busy={busy} disabled={!status?.enabled} onClick={() => void perform(async () => {
        const next = await api.icloud_backup_now(); toast(t("backup.saved"), "success"); return next;
      }, true)}>{t(saving ? "backup.saving" : "backup.now")}</Button>
      <Button icon="arrows-clockwise" busy={refreshing} disabled={busy} onClick={() => void load(true)}>{t("backup.refresh")}</Button>
      <Button icon="folder-open" disabled={busy} onClick={() => void perform(async () => {
        const file = await chooseBackupFile();
        if (file) setRestore(file);
      })}>{t("backup.selectFile")}</Button>
    </div>
    {(error || refreshError || status?.error) && <div role="alert" className={s.error}>{error || refreshError || status?.error}</div>}
    <p className={s.hint}>{t("backup.last")}: {status?.last_at ? formatDateTime(t.locale, status.last_at) : t("backup.never")}</p>
    <Section title={t("backup.history")}><Group>
      {!status?.entries.length && <div className={s.empty}>{t("backup.empty")}</div>}
      {status?.entries.map((entry) => <div className={s.entry} key={entry.name}>
        <div className={s.meta}><div className={s.time}>{formatDateTime(t.locale, entry.created_at)} · {formatBytes(entry.size)}</div>
          <div className={s.name}>{entry.name}</div>
          <div className={`${s.state} ${entry.transfer === "uploaded" ? s.good : entry.transfer === "uploading" ? s.uploading : ["error", "timeout"].includes(entry.transfer) ? s.failed : ""}`} role="status">
            {entry.transfer === "uploading" ? <Spinner /> : <Icon name={entry.transfer === "uploaded" ? "check-circle" : ["error", "timeout"].includes(entry.transfer) ? "warning-circle" : entry.transfer === "pending" ? "clock" : "question"} size={14} />}
            <span>{t(entry.transfer === "uploaded" ? "backup.uploaded" : entry.transfer === "uploading" ? "backup.uploading" : entry.transfer === "pending" ? "backup.pending" : entry.transfer === "error" ? "backup.error" : entry.transfer === "timeout" ? "backup.checkFailed" : "backup.unknown")}</span>
          </div>
        </div><Button disabled={busy} onClick={() => setRestore(entry.name)}>{t("backup.restore")}</Button>
      </div>)}
    </Group></Section>
    <p className={s.hint}>{t("backup.contents")}</p><p className={s.hint}>{t("backup.note")}</p>
    {restore && <BackupRestore path={restore} replace onClose={() => setRestore(null)} />}
  </div></div></div>;
}

export function BackupRestore({ path, replace = false, onClose }: { path: string; replace?: boolean; onClose: () => void }) {
  const t = useT();
  const [password, setPassword] = useState("");
  const [currentPassword, setCurrentPassword] = useState("");
  const [confirmed, setConfirmed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const restore = async () => {
    if (busy || !password || (replace && (!confirmed || !currentPassword))) return;
    setBusy(true); setError(null);
    try {
      const rollback = await api.icloud_backup_restore(path, password, replace, replace ? currentPassword : null);
      setPassword(""); setCurrentPassword("");
      if (rollback) {
        await api.show_backup_in_finder(rollback).catch(() => {});
      }
      if (isTauri()) { window.location.reload(); return; }
      await enterUnlocked(); useApp.setState({ settingsOpen: false });
      toast(t("backup.restored"), "success"); onClose();
    } catch (e) { setError(errorMessage(t, e)); setBusy(false); }
  };
  return <Dialog title={t("backup.restoreTitle")} body={replace ? t("backup.replaceHint") : t("backup.restoreHint")}
    onClose={busy ? undefined : onClose} actions={<>
      <Button disabled={busy} onClick={onClose}>{t("backup.cancel")}</Button>
      <Button variant="primary" busy={busy} disabled={!password || (replace && (!confirmed || !currentPassword))} onClick={() => void restore()}>{t("backup.restore")}</Button>
    </>}>
    <div className={s.fields}>
      <div className={s.path}>{path}</div>
      {replace && <label className={s.field}>{t("backup.currentPassword")}<TextField secret autoComplete="current-password" value={currentPassword} disabled={busy} onChange={(e) => setCurrentPassword(e.target.value)} /></label>}
      <label className={s.field}>{t("backup.password")}<TextField secret autoFocus={!replace} autoComplete="off" value={password} disabled={busy} onChange={(e) => setPassword(e.target.value)} /></label>
      <p className={s.hint}>{t("backup.restoreHint")}</p>
      {replace && <label className={s.check}><input type="checkbox" checked={confirmed} disabled={busy} onChange={(e) => setConfirmed(e.target.checked)} />{t("backup.confirmReplace")}</label>}
      {error && <div role="alert" className={s.error}>{error}</div>}
    </div>
  </Dialog>;
}
