import { useEffect, useState } from "react";
import { useApp } from "@/app/store";
import { api } from "@/ipc/api";
import { SyncConflictsView } from "./SyncConflictsView";
import { SyncStatusView } from "./SyncStatusView";
import { SyncWizard } from "./SyncWizard";
import { ICloudBackup } from "./ICloudBackup";
import s from "./Sync.module.css";

/**
 * Cloud Sync (design §05): the setup wizard while sync is off, otherwise the status page, with a
 * review screen for automatically resolved conflicts (spec §6.4).
 */
export function SyncPage() {
  const mac = useApp((st) => st.info.platform === "macos");
  const [backupOpen, setBackupOpen] = useState<boolean | null>(() => mac ? useApp.getState().icloudBackup?.enabled ?? null : false);
  const sync = useApp((st) => st.sync);
  const [reviewing, setReviewing] = useState(false);
  const kind = sync?.kind;

  // Open the enabled backup method when returning to this page, including after restart.
  useEffect(() => {
    if (!mac) return;
    const known = useApp.getState().icloudBackup;
    if (known) { setBackupOpen(known.enabled); return; }
    let active = true;
    void api.icloud_backup_status().then(
      (status) => { if (active) { useApp.getState().setICloudBackup(status); setBackupOpen(status.enabled); } },
      () => { if (active) setBackupOpen(false); },
    );
    return () => { active = false; };
  }, [mac]);

  // Leaving sync (disconnect) must not keep us on the conflicts screen next time.
  useEffect(() => {
    if (kind === "none") setReviewing(false);
  }, [kind]);

  if (mac && backupOpen) return <ICloudBackup onBack={() => setBackupOpen(false)} />;
  return !sync || (mac && backupOpen === null) ? <div className={s.page} /> : sync.kind === "none" ? <SyncWizard />
    : reviewing ? <SyncConflictsView onClose={() => setReviewing(false)} />
    : <SyncStatusView status={sync} onReview={() => setReviewing(true)}
      onBackup={mac ? () => setBackupOpen(true) : undefined} />;
}
