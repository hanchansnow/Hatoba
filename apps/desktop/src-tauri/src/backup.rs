//! Device-local iCloud backup settings, scheduling and encrypted snapshot publication.
use crate::dto::{ICloudBackupEntry, ICloudBackupStatus};
use crate::error::{AppError, AppResult};
use crate::platform::icloud;
use crate::state::{AppState, blocking};
use crate::sync::Trigger;
use hatoba_core::backup::{BackupFile, read_backup, write_backup};
use hatoba_core::sync::Backoff;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

const CONFIG_KEY: &str = "icloud_backup";
fn default_retention() -> u32 {
    5
}

#[derive(Default)]
pub struct Backups {
    pub gate: tokio::sync::Mutex<()>,
    error: Mutex<Option<String>>,
    files: Mutex<HashMap<PathBuf, FileStatus>>,
    wake: tokio::sync::Notify,
    pending: Mutex<Option<Trigger>>,
}

const MAX_CHECKS: u8 = 5;

struct FileStatus {
    metadata: std::fs::Metadata,
    transfer: &'static str,
    checks: u8,
}
impl FileStatus {
    fn new(metadata: std::fs::Metadata) -> Self {
        Self {
            metadata,
            transfer: "pending",
            checks: 0,
        }
    }
    fn check(&mut self, path: &Path, retry: bool) -> &'static str {
        if retry && self.transfer != "uploaded" {
            self.checks = 0;
        }
        if self.transfer == "uploaded" || self.checks >= MAX_CHECKS {
            return self.transfer;
        }
        self.checks += 1;
        let transfer = icloud::inspect(path)
            .map(|state| state.transfer())
            .unwrap_or("unknown");
        self.transfer = if transfer == "uploaded" {
            "uploaded"
        } else if self.checks == MAX_CHECKS {
            if transfer == "error" {
                "error"
            } else {
                "timeout"
            }
        } else if transfer == "error" {
            // A fresh publication can carry an earlier upload error while macOS queues a retry.
            "pending"
        } else {
            transfer
        };
        self.transfer
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    automatic: bool,
    last_at: Option<i64>,
    retention: u32,
    last_digest: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: false,
            retention: default_retention(),
            last_at: None,
            last_digest: None,
        }
    }
}

fn config(state: &AppState) -> AppResult<Config> {
    let value = state
        .vault()
        .local_prefs()?
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    parse_config(
        value
            .get(CONFIG_KEY)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
}
fn parse_config(mut value: serde_json::Value) -> AppResult<Config> {
    // Earlier builds used a selected directory as the implicit enable switch.
    // Migrate that intent, but never reuse a custom save location.
    if value.get("enabled").is_none() {
        let enabled = value
            .get("directory")
            .and_then(|v| v.as_str())
            .is_some_and(|p| !p.is_empty());
        if let Some(object) = value.as_object_mut() {
            object.insert("enabled".into(), enabled.into());
        }
    }
    let mut cfg: Config = serde_json::from_value(value)
        .map_err(|_| AppError::invalid("backup", "Invalid backup preferences"))?;
    if cfg.retention == 0 {
        cfg.retention = default_retention();
    }
    Ok(cfg)
}
fn save_config(state: &AppState, cfg: &Config) -> AppResult<()> {
    let mut vault = state.vault();
    let mut value = vault
        .local_prefs()?
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    value[CONFIG_KEY] = serde_json::to_value(cfg).map_err(|e| AppError::internal(e.to_string()))?;
    vault.set_local_prefs(&value.to_string())?;
    Ok(())
}

/// Filenames carry only timestamps, device IDs and random IDs, never host names.
fn filename(device: &str, at: i64) -> String {
    let date = chrono::DateTime::from_timestamp_millis(at)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y%m%dT%H%M%S");
    format!(
        "hatoba-{date}-{at}-{device}-{}.hatoba-backup",
        uuid::Uuid::now_v7()
    )
}
fn file_time(name: &str) -> Option<i64> {
    let body = name
        .strip_prefix("hatoba-")?
        .strip_suffix(".hatoba-backup")?;
    let mut fields = body.splitn(3, '-');
    chrono::NaiveDateTime::parse_from_str(fields.next()?, "%Y%m%dT%H%M%S").ok()?;
    let at = fields.next()?.parse().ok()?;
    let ids = fields.next()?;
    if !ids.is_ascii() || ids.len() != 73 || ids.as_bytes()[36] != b'-' {
        return None;
    }
    uuid::Uuid::parse_str(&ids[..36]).ok()?;
    uuid::Uuid::parse_str(&ids[37..]).ok()?;
    Some(at)
}

pub fn configure(
    state: &AppState,
    enabled: bool,
    automatic: bool,
    retention: Option<u32>,
) -> AppResult<()> {
    if !cfg!(target_os = "macos") {
        return Err(AppError::invalid("platform", "macOS only"));
    }
    let old = config(state)?;
    let retention = retention.unwrap_or(old.retention);
    if retention == 0 {
        return Err(AppError::invalid("retention", "Keep at least one snapshot"));
    }
    if enabled {
        let root = icloud::backup_directory(true)?;
        prune(&root, retention, None)?;
    }
    save_config(
        state,
        &Config {
            enabled,
            automatic,
            retention,
            ..old
        },
    )?;
    *state
        .backups
        .error
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

// Track immutable snapshot contents. iCloud status/xattr changes must not restart the check budget.
fn unchanged_file(previous: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            previous.dev(),
            previous.ino(),
            previous.len(),
            previous.mtime(),
            previous.mtime_nsec(),
        ) == (
            current.dev(),
            current.ino(),
            current.len(),
            current.mtime(),
            current.mtime_nsec(),
        )
    }
    #[cfg(not(unix))]
    {
        previous.len() == current.len()
            && previous.modified().ok() == current.modified().ok()
            && previous.created().ok() == current.created().ok()
    }
}

pub fn status(state: &AppState, retry: bool) -> AppResult<ICloudBackupStatus> {
    let cfg = config(state)?;
    let mut entries = Vec::new();
    let mut cached = state
        .backups
        .files
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut present = HashSet::new();
    let mut directory_error = state
        .backups
        .error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match icloud::backup_directory(false).and_then(|p| Ok(std::fs::read_dir(p)?)) {
        Err(error) => {
            if cfg.enabled {
                directory_error = Some(error.detail);
            }
        }
        Ok(files) => {
            for entry in files.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(created_at) = file_time(&name) else {
                    continue;
                };
                let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    continue;
                }
                let path = entry.path();
                present.insert(path.clone());
                let file = cached
                    .entry(path.clone())
                    .or_insert_with(|| FileStatus::new(metadata.clone()));
                if !unchanged_file(&file.metadata, &metadata) {
                    *file = FileStatus::new(metadata.clone());
                }
                let transfer = file.check(&path, retry);
                entries.push(ICloudBackupEntry {
                    name,
                    created_at,
                    size: metadata.len(),
                    transfer: transfer.into(),
                });
            }
        }
    }
    if directory_error.is_none() {
        cached.retain(|path, _| present.contains(path));
    }
    entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(ICloudBackupStatus {
        enabled: cfg.enabled,
        automatic: cfg.automatic,
        retention: cfg.retention,
        last_at: cfg.last_at,
        entries,
        error: directory_error,
    })
}

pub fn selected_file(name: &str) -> AppResult<PathBuf> {
    if file_time(name).is_none()
        || Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name)
    {
        return Err(AppError::invalid("name", "Invalid backup name"));
    }
    let root = icloud::backup_directory(false)?;
    let path = root.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AppError::invalid(
                "name",
                "Backup must not be a symbolic link",
            ));
        }
        // Foundation gets a chance to materialize an iCloud placeholder before POSIX reading.
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    Ok(path)
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn staging(app: &AppHandle) -> AppResult<PathBuf> {
    let directory = app.path().app_local_data_dir()?.join("backup-staging");
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("{}.hatoba", uuid::Uuid::now_v7())))
}

pub fn read_verified(
    app: &AppHandle,
    source: &Path,
    password: &str,
) -> AppResult<hatoba_core::backup::VerifiedBackup> {
    let temporary = Temporary(staging(app)?);
    icloud::read(source, &temporary.0)?;
    Ok(read_backup(&temporary.0)?.verify(password)?)
}

fn fingerprint(file: &mut BackupFile) -> AppResult<String> {
    let at = file.created_at;
    file.created_at = 0;
    let encoded = serde_json::to_vec(&*file).map_err(|e| AppError::internal(e.to_string()))?;
    file.created_at = at;
    Ok(Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// Only app snapshots participate in retention; restore safety files live elsewhere.
fn prune(root: &Path, retention: u32, keep: Option<&Path>) -> AppResult<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(at) = file_time(&name) else {
            continue;
        };
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            files.push((at, entry.path()));
        }
    }
    // Keep the newly published file even if the device clock moved backwards.
    files.sort_by(|a, b| {
        (Some(b.1.as_path()) == keep)
            .cmp(&(Some(a.1.as_path()) == keep))
            .then_with(|| b.0.cmp(&a.0))
            .then_with(|| b.1.cmp(&a.1))
    });
    for (_, path) in files.into_iter().skip(retention as usize) {
        icloud::remove(&path)?;
    }
    Ok(())
}

pub async fn run(app: AppHandle, automatic: bool) -> AppResult<()> {
    let state = app.state::<AppState>();
    let _gate = if automatic {
        match state.backups.gate.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(()),
        }
    } else {
        state.backups.gate.lock().await
    };
    let cfg = config(&state)?;
    if automatic && !(cfg.enabled && cfg.automatic) {
        return Ok(());
    }
    if !cfg.enabled {
        return Err(AppError::invalid("backup", "Enable iCloud Backup first"));
    }
    let (mut file, device) = {
        let vault = state.vault();
        if !vault.is_unlocked() {
            return if automatic {
                Ok(())
            } else {
                Err(AppError::locked())
            };
        }
        (vault.backup_snapshot()?, vault.device_id())
    };
    let work_app = app.clone();
    let outcome = blocking(move || {
        let digest = fingerprint(&mut file)?;
        if automatic && cfg.last_digest.as_deref() == Some(&digest) {
            return Ok(None);
        }
        let root = icloud::backup_directory(true)?;
        let temporary = Temporary(staging(&work_app)?);
        write_backup(&file, &temporary.0)?;
        // Structural validation of the bytes written before publishing an immutable snapshot.
        read_backup(&temporary.0)?;
        let target = root.join(filename(&device, file.created_at));
        icloud::publish(&temporary.0, &target)?;
        prune(&root, cfg.retention, Some(&target))?;
        Ok(Some((digest, file.created_at)))
    })
    .await;
    match outcome {
        Ok(Some((digest, at))) => {
            let mut cfg = config(&state)?;
            cfg.last_at = Some(at);
            cfg.last_digest = Some(digest);
            save_config(&state, &cfg)?;
            *state
                .backups
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            if automatic {
                emit_status(&app).await;
            }
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => {
            *state
                .backups
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(error.detail.clone());
            if automatic {
                emit_status(&app).await;
            }
            Err(error)
        }
    }
}

async fn emit_status(app: &AppHandle) {
    let handle = app.clone();
    if let Ok(status) = blocking(move || status(&handle.state::<AppState>(), false)).await {
        let _ = app.emit("backup://changed", status);
    }
}

pub fn trigger(app: &AppHandle, trigger: Trigger) {
    if !cfg!(target_os = "macos") || trigger == Trigger::Manual {
        return;
    }
    let state = app.state::<AppState>();
    let mut pending = state
        .backups
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if pending.is_none() || trigger != Trigger::LocalChange {
        *pending = Some(trigger);
    }
    state.backups.wake.notify_one();
}

pub fn spawn_scheduler(app: AppHandle) {
    if !cfg!(target_os = "macos") {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let mut backoff = Backoff::default();
        let mut retry_at: Option<tokio::time::Instant> = None;
        loop {
            let state = app.state::<AppState>();
            let interval = std::time::Duration::from_secs(60);
            let wait = retry_at.map_or(interval, |at| {
                at.saturating_duration_since(tokio::time::Instant::now())
                    .min(interval)
            });
            let woke = tokio::time::timeout(wait, state.backups.wake.notified())
                .await
                .is_ok();
            let trigger = state
                .backups
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if woke && trigger == Some(Trigger::LocalChange) {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                state
                    .backups
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
            }
            match run(app.clone(), true).await {
                Ok(()) => {
                    backoff.reset();
                    retry_at = None;
                }
                Err(_) => retry_at = Some(tokio::time::Instant::now() + backoff.next_delay()),
            }
        }
    });
}
