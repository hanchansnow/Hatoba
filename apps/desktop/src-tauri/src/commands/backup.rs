//! macOS encrypted backups: iCloud management and explicit restore.
use crate::dto::ICloudBackupStatus;
use crate::error::{AppError, AppResult};
use crate::{
    backup,
    state::{AppState, blocking},
};
use hatoba_core::{SecretStore, platform::secret_keys};
use std::path::Path;
use tauri::{AppHandle, Manager, State};
use zeroize::Zeroizing;

#[tauri::command]
#[specta::specta]
pub async fn icloud_backup_status(
    app: AppHandle,
    retry: Option<bool>,
) -> AppResult<ICloudBackupStatus> {
    let state = app.state::<AppState>();
    let _gate = state.backups.gate.lock().await;
    let handle = app.clone();
    let status =
        blocking(move || backup::status(&handle.state::<AppState>(), retry.unwrap_or(false)))
            .await?;
    if status
        .devices
        .iter()
        .flat_map(|device| &device.entries)
        .any(|entry| !["uploaded", "error", "timeout"].contains(&entry.transfer.as_str()))
    {
        backup::start_monitor(app.clone());
    }
    Ok(status)
}
#[tauri::command]
#[specta::specta]
pub async fn icloud_backup_configure(
    app: AppHandle,
    enabled: bool,
    automatic: bool,
    retention: Option<u32>,
) -> AppResult<()> {
    let state = app.state::<AppState>();
    let _gate = state.backups.gate.lock().await;
    let handle = app.clone();
    blocking(move || backup::configure(&handle.state::<AppState>(), enabled, automatic, retention))
        .await?;
    drop(_gate);
    backup::trigger(&app, crate::sync::Trigger::Unlock);
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub async fn icloud_backup_now(app: AppHandle) -> AppResult<ICloudBackupStatus> {
    backup::run(app.clone(), false).await?;
    icloud_backup_status(app, None).await
}

#[tauri::command]
#[specta::specta]
pub async fn icloud_backup_restore(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    password: String,
    replace: bool,
    current_password: Option<String>,
) -> AppResult<Option<String>> {
    let password = Zeroizing::new(password);
    let current_password = current_password.map(Zeroizing::new);
    if !cfg!(target_os = "macos") {
        return Err(AppError::invalid("platform", "macOS only"));
    }
    let _operation = state.lifecycle_operation()?;
    let generation = state.vault().lock_generation();
    let _gate = state.backups.gate.lock().await;
    {
        let vault = state.vault();
        if vault.status().initialized && (!replace || !vault.is_unlocked()) {
            return Err(AppError::invalid(
                "restore",
                "Unlock the vault and confirm replacement first",
            ));
        }
    }
    if state.vault().status().initialized {
        let current = current_password
            .ok_or_else(|| AppError::invalid("password", "Current master password required"))?;
        let vault = state.vault.clone();
        let valid = blocking(move || {
            Ok(vault
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .verify_password(&current)?)
        })
        .await?;
        if !valid {
            return Err(AppError::new(
                crate::error::ErrorCode::WrongPassword,
                "Wrong current master password",
            ));
        }
    }
    let source = if replace && !Path::new(&path).is_absolute() {
        backup::selected_file(&state, &path)?
    } else {
        if !Path::new(&path).is_absolute() {
            return Err(AppError::invalid(
                "path",
                "Select an absolute backup file path",
            ));
        }
        Path::new(&path).to_path_buf()
    };
    let work_app = app.clone();
    let verified = blocking(move || backup::read_verified(&work_app, &source, &password)).await?;
    // No current data or sessions are touched until the complete backup has been verified.
    // Stop the old sync connection and hold its round guard until the replacement commits.
    crate::lock::lock_vault(&app, crate::dto::LockReason::Manual).await;
    let expected_generation = generation.wrapping_add(1);
    for (id, session) in state.ssh.take_all() {
        state.ai.file_writes_ended(&id).await;
        session.close().await;
    }
    let round = state.sync.end_connection().await;
    let rollback = if state.vault().status().initialized {
        let directory = app.path().app_local_data_dir()?.join("restore-safety");
        let snapshot = state.vault().backup_snapshot()?;
        Some(
            blocking(move || {
                let target = backup::write_restore_safety(&directory, &snapshot)?;
                Ok(target.to_string_lossy().into_owned())
            })
            .await?,
        )
    } else {
        None
    };
    {
        let mut vault = state.vault();
        state.ai.stop_all(&mut vault);
        vault
            .restore_verified_backup_preserving_lock(verified, replace, expected_generation)
            .map_err(|error| AppError::from(error).with_safety_backup(rollback.clone()))?;
    }
    // Invalidate any backend captured before the swap while rounds are still excluded.
    state.sync.set_backend(None);
    state.deploy.clear();
    // A restored vault has a new device identity; credentials of the old connection are obsolete.
    let _ = hatoba_core::sync::clear_session(state.secrets.as_ref());
    let _ = state.secrets.delete(secret_keys::D1_API_TOKEN);
    let _ = crate::platform::biometric::remove(state.secrets.as_ref());
    drop(round);
    crate::commands::vault::after_unlock(&app, &state);
    crate::sync::emit_status(&app);
    Ok(rollback)
}

#[tauri::command]
#[specta::specta]
pub fn show_backup_in_finder(app: AppHandle, path: String) -> AppResult<()> {
    let root = app
        .path()
        .app_local_data_dir()?
        .join("restore-safety")
        .canonicalize()?;
    let file = Path::new(&path).canonicalize()?;
    if !file.starts_with(root) {
        return Err(AppError::invalid("path", "Not a restore safety backup"));
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(file)
            .spawn()?;
    }
    Ok(())
}
