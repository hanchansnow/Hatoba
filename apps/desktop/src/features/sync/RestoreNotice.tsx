import { useApp } from "@/app/store";
import { Button, Spinner } from "@/components/controls";
import { Dialog } from "@/components/overlay";
import { useT } from "@/i18n";

/** Survives the lock event which unmounts the restore form; contains no vault contents. */
export function RestoreNotice() {
  const t = useT();
  const operation = useApp((state) => state.backupRestore);
  if (!operation) return null;
  const close = () => useApp.setState({ backupRestore: null });
  return <Dialog whileLocked title={t("backup.restoreTitle")} onClose={operation.running ? undefined : close}
    actions={operation.running ? <Spinner /> : <Button autoFocus onClick={close}>{t("btn.close")}</Button>}>
    <p role={operation.running ? "status" : "alert"}>{operation.running ? t("backup.restoring") : operation.error}</p>
  </Dialog>;
}
