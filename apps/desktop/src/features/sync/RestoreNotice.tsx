import { useState } from "react";
import { errorMessage } from "@/app/errors";
import { useApp } from "@/app/store";
import { Button, Spinner } from "@/components/controls";
import { Dialog, toast } from "@/components/overlay";
import { useT } from "@/i18n";
import { api } from "@/ipc/api";
import { writeRestoreResult } from "./restoreState";
import s from "./ICloudBackup.module.css";

/** Survives both the lock transition and the reload after successful restoration. */
export function RestoreNotice() {
  const t = useT();
  const operation = useApp((state) => state.backupRestore);
  const [opening, setOpening] = useState(false);
  if (!operation) return null;
  const close = () => { writeRestoreResult(null); useApp.setState({ backupRestore: null }); };
  const reveal = async () => {
    if (!operation.safetyBackup || opening) return;
    setOpening(true);
    try { await api.show_backup_in_finder(operation.safetyBackup); }
    catch (error) { toast(errorMessage(t, error), "error"); }
    finally { setOpening(false); }
  };
  return <Dialog whileLocked title={t(operation.completed ? "backup.restored" : "backup.restoreTitle")}
    onClose={operation.running ? undefined : close}
    actions={operation.running ? <Spinner /> : <>
      {operation.safetyBackup && <Button busy={opening} onClick={() => void reveal()}>{t("backup.showSafety")}</Button>}
      <Button autoFocus onClick={close}>{t("btn.close")}</Button>
    </>}>
    <p role={operation.running || operation.completed ? "status" : "alert"}>
      {operation.running ? t("backup.restoring") : operation.completed ? t("backup.restoredHint") : operation.failedItems?.length ? t("backup.authenticationFailed") : operation.error}
    </p>
    {!!operation.failedItems?.length && <><p>{t("backup.failedRecords")}</p><pre className={s.path}>{operation.failedItems.join("\n")}</pre></>}
    {operation.safetyBackup && <><p>{t("backup.safetySaved")}</p><div className={s.path}>{operation.safetyBackup}</div></>}
  </Dialog>;
}
