//! Device-local iCloud backup settings, scheduling and encrypted snapshot publication.
use crate::dto::{ICloudBackupDevice, ICloudBackupEntry, ICloudBackupStatus};
use crate::error::{AppError, AppResult};
use crate::platform::icloud;
use crate::state::{AppState, blocking};
use crate::sync::Trigger;
use hatoba_core::backup::{BackupFile, read_backup, write_backup};
use hatoba_core::crypto::{device_aad, open_json, seal};
use hatoba_core::platform::DeviceInfo;
use hatoba_core::sync::Backoff;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use zeroize::Zeroizing;

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
    monitoring: AtomicBool,
}

const MAX_CHECKS: u8 = 5;

struct FileStatus {
    metadata: std::fs::Metadata,
    transfer: &'static str,
    checks: u8,
    checked_at: Option<Instant>,
}
impl FileStatus {
    fn new(metadata: std::fs::Metadata) -> Self {
        Self {
            metadata,
            transfer: "pending",
            checks: 0,
            checked_at: None,
        }
    }
    fn check(&mut self, path: &Path, retry: bool) -> &'static str {
        if retry && self.transfer != "uploaded" {
            self.checks = 0;
            self.checked_at = None;
        }
        if self.transfer == "uploaded" || self.checks >= MAX_CHECKS {
            return self.transfer;
        }
        if self
            .checked_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
        {
            return self.transfer;
        }
        self.checked_at = Some(Instant::now());
        self.record(
            icloud::inspect(path)
                .map(|state| state.transfer())
                .unwrap_or("unknown"),
        )
    }
    fn record(&mut self, transfer: &'static str) -> &'static str {
        self.checks += 1;
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
    last_name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: false,
            retention: default_retention(),
            last_at: None,
            last_digest: None,
            last_name: None,
        }
    }
}

fn config(state: &AppState) -> AppResult<Config> {
    vault_config(&state.vault())
}
fn vault_config(vault: &hatoba_core::Vault) -> AppResult<Config> {
    let value = vault
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
    save_vault_config(&mut state.vault(), cfg)
}
fn save_vault_config(vault: &mut hatoba_core::Vault, cfg: &Config) -> AppResult<()> {
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
fn file_identity(name: &str) -> Option<(i64, &str)> {
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
    Some((at, &ids[..36]))
}

/// Device identity belongs to this vault installation, not to the physical Mac. Restoring
/// creates a new identity, so independent vaults on one machine never share retention.
fn device_directory(root: &Path, device: &str, create: bool) -> AppResult<PathBuf> {
    if uuid::Uuid::parse_str(device)
        .ok()
        .map(|id| id.to_string())
        .as_deref()
        != Some(device)
    {
        return Err(AppError::invalid(
            "backup",
            "Invalid backup device identity",
        ));
    }
    let directory = root.join(device);
    if create {
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || directory.canonicalize()? != directory
    {
        return Err(AppError::invalid("backup", "Invalid device backup folder"));
    }
    Ok(directory)
}

fn visible_name(name: &str) -> &str {
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".icloud"))
        .unwrap_or(name)
}
fn snapshot_present(directory: &Path, name: &str) -> bool {
    [
        directory.join(name),
        directory.join(format!(".{name}.icloud")),
    ]
    .iter()
    .any(|path| {
        std::fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    })
}

fn snapshots(directory: &Path, device: &str) -> AppResult<Vec<(i64, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((at, owner)) = file_identity(visible_name(&name)) else {
            continue;
        };
        if owner != device {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            files.push((at, entry.path()));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(files)
}

fn migrate_legacy(root: &Path, device: &str, directory: &Path) -> AppResult<()> {
    migrate_with(root, device, directory, icloud::move_file)
}
fn migrate_with(
    root: &Path,
    device: &str,
    directory: &Path,
    mut move_file: impl FnMut(&Path, &Path) -> AppResult<()>,
) -> AppResult<()> {
    // Never touch root-level backups belonging to another installation or unknown files.
    for (_, source) in snapshots(root, device)? {
        // An evicted legacy file remains listed and restorable without moving its placeholder.
        if source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name != visible_name(name))
        {
            continue;
        }
        let target = directory.join(source.file_name().expect("snapshot filename"));
        if !target.try_exists()? {
            move_file(&source, &target)?;
        }
    }
    Ok(())
}

/// Destructive work only runs after the setting has been durably saved.
fn save_then_cleanup(
    save: impl FnOnce() -> AppResult<()>,
    cleanup: impl FnOnce() -> AppResult<()>,
) -> AppResult<Option<String>> {
    save()?;
    Ok(cleanup()
        .err()
        .map(|error| format!("Backup settings saved; cleanup failed: {}", error.detail)))
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
    let device = state.vault().device_id();
    let location = if enabled {
        let root = icloud::backup_directory(true)?;
        let directory = device_directory(&root, &device, true)?;
        Some((root, directory))
    } else {
        None
    };
    let cfg = Config {
        enabled,
        automatic,
        retention,
        ..old
    };
    let error = save_then_cleanup(
        || save_config(state, &cfg),
        || {
            if let Some((root, directory)) = location {
                migrate_legacy(&root, &device, &directory)?;
                cleanup(state, &directory, &device, &cfg)?;
            }
            Ok(())
        },
    )?;
    *state
        .backups
        .error
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = error;
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

fn read_entries(
    directory: &Path,
    device: &str,
    cached: &mut HashMap<PathBuf, FileStatus>,
    present: &mut HashSet<PathBuf>,
    retry: bool,
) -> AppResult<Vec<ICloudBackupEntry>> {
    let mut entries = Vec::new();
    for (created_at, physical) in snapshots(directory, device)? {
        let name = physical
            .file_name()
            .expect("snapshot filename")
            .to_string_lossy();
        let name = visible_name(&name).to_string();
        let metadata = std::fs::symlink_metadata(&physical)?;
        let path = directory.join(&name);
        present.insert(path.clone());
        let file = cached
            .entry(path.clone())
            .or_insert_with(|| FileStatus::new(metadata.clone()));
        if !unchanged_file(&file.metadata, &metadata) {
            *file = FileStatus::new(metadata.clone());
        }
        entries.push(ICloudBackupEntry {
            name,
            created_at,
            size: metadata.len(),
            transfer: file.check(&path, retry).into(),
        });
    }
    Ok(entries)
}
/// Same vault-key envelope and device AAD as the sync device-name policy (§6.3).
fn sealed_device_name(vault: &hatoba_core::Vault, device: &str) -> AppResult<String> {
    let key = vault.vault_key_copy()?;
    let plain = Zeroizing::new(
        serde_json::to_vec(&crate::sync::device_info())
            .map_err(|error| AppError::internal(error.to_string()))?,
    );
    Ok(seal(&key, device_aad(device).as_bytes(), &plain)?.to_json())
}
fn read_device_name(
    directory: &Path,
    device: &str,
    key: Option<&hatoba_core::crypto::Key32>,
) -> Option<DeviceInfo> {
    let path = directory.join("device.hatoba-device");
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let sealed = std::str::from_utf8(&bytes).ok()?;
    let plain = open_json(key?, device_aad(device).as_bytes(), sealed).ok()?;
    serde_json::from_slice(&plain).ok()
}

fn last_uploaded(entries: &[ICloudBackupEntry]) -> Option<i64> {
    entries
        .iter()
        .filter(|entry| entry.transfer == "uploaded")
        .map(|entry| entry.created_at)
        .max()
}
fn device_folders(root: &Path) -> AppResult<Vec<(String, PathBuf)>> {
    let mut devices = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if uuid::Uuid::parse_str(&id)
            .ok()
            .map(|uuid| uuid.to_string())
            .as_deref()
            != Some(&id)
        {
            continue;
        }
        // An unrelated directory or a symlink is never traversed.
        if let Ok(path) = device_directory(root, &id, false) {
            devices.push((id, path));
        }
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(devices)
}
fn legacy_devices(root: &Path) -> AppResult<HashSet<String>> {
    let mut devices = HashSet::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((_, owner)) = file_identity(visible_name(&name)) {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                devices.insert(owner.to_string());
            }
        }
    }
    Ok(devices)
}

pub fn status(state: &AppState, retry: bool) -> AppResult<ICloudBackupStatus> {
    let cfg = config(state)?;
    let (current, key) = {
        let vault = state.vault();
        (vault.device_id(), vault.vault_key_copy().ok())
    };
    let mut error = state
        .backups
        .error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut devices = Vec::new();
    let root = icloud::backup_directory(cfg.enabled);
    if let Ok(root) = &root {
        if cfg.enabled {
            let migrated = device_directory(root, &current, true)
                .and_then(|directory| migrate_legacy(root, &current, &directory));
            if let Err(failure) = migrated {
                error = Some(failure.detail);
            }
        }
        let mut present = HashSet::new();
        let mut cached = state
            .backups
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut folders = device_folders(root)?;
        let legacy = legacy_devices(root)?;
        for device in &legacy {
            if !folders.iter().any(|(id, _)| id == device) {
                folders.push((device.clone(), root.join(device)));
            }
        }
        let mut complete = true;
        for (device_id, directory) in folders {
            let result: AppResult<Vec<ICloudBackupEntry>> = (|| {
                let mut entries = if device_directory(root, &device_id, false).is_ok() {
                    read_entries(&directory, &device_id, &mut cached, &mut present, retry)?
                } else {
                    Vec::new()
                };
                if legacy.contains(&device_id) {
                    let mut old = read_entries(root, &device_id, &mut cached, &mut present, retry)?;
                    for entry in &mut old {
                        entry.name = format!("legacy/{}", entry.name);
                    }
                    entries.extend(old);
                    entries.sort_by(|a, b| {
                        b.created_at
                            .cmp(&a.created_at)
                            .then_with(|| b.name.cmp(&a.name))
                    });
                }
                Ok(entries)
            })();
            complete &= result.is_ok();
            let (entries, failure) = match result {
                Ok(entries) => (entries, None),
                Err(failure) => (Vec::new(), Some(failure.detail)),
            };
            let is_current = device_id == current;
            let info = device_directory(root, &device_id, false)
                .ok()
                .and_then(|directory| read_device_name(&directory, &device_id, key.as_ref()));
            devices.push(ICloudBackupDevice {
                name: if is_current {
                    crate::sync::device_info().name
                } else {
                    info.as_ref()
                        .map(|info| info.name.clone())
                        .unwrap_or_else(|| device_id.chars().take(8).collect())
                },
                platform: info
                    .map(|info| info.platform)
                    .unwrap_or_else(|| "macOS".into()),
                current: is_current,
                last_at: entries.first().map(|entry| entry.created_at),
                last_uploaded_at: last_uploaded(&entries),
                device_id,
                entries,
                error: failure,
            });
        }
        if complete {
            cached.retain(|path, _| present.contains(path));
        }
        drop(cached);
        if cfg.enabled
            && let Ok(directory) = device_directory(root, &current, false)
        {
            match cleanup(state, &directory, &current, &cfg) {
                Err(failure) => {
                    error = Some(format!(
                        "Backup settings saved; cleanup failed: {}",
                        failure.detail
                    ))
                }
                Ok(()) => {
                    if error.as_ref().is_some_and(|text| {
                        text.starts_with("Backup settings saved; cleanup failed:")
                    }) {
                        error = None;
                    }
                }
            }
            if let Some(device) = devices.iter_mut().find(|device| device.current) {
                device.entries.retain(|entry| {
                    entry.name.strip_prefix("legacy/").map_or_else(
                        || snapshot_present(&directory, &entry.name),
                        |name| snapshot_present(root, name),
                    )
                });
                // Protect the last publication even when the system clock moved backwards.
                device
                    .entries
                    .sort_by_key(|entry| cfg.last_name.as_deref() != Some(entry.name.as_str()));
                device.last_uploaded_at = last_uploaded(&device.entries);
            }
        }
    } else if cfg.enabled {
        error = Some(root.expect_err("failed directory").detail);
    }
    devices.sort_by_key(|device| !device.current);
    let entries = devices
        .iter()
        .find(|device| device.current)
        .map(|device| device.entries.clone())
        .unwrap_or_default();
    if let Some(failure) = devices
        .iter()
        .find(|device| device.current)
        .and_then(|device| device.error.clone())
    {
        error = Some(failure);
    }
    Ok(ICloudBackupStatus {
        enabled: cfg.enabled,
        automatic: cfg.automatic,
        retention: cfg.retention,
        last_at: cfg.last_at,
        last_uploaded_at: last_uploaded(&entries),
        entries,
        devices,
        error,
    })
}

pub fn selected_file(state: &AppState, name: &str) -> AppResult<PathBuf> {
    let current = state.vault().device_id();
    let components: Vec<_> = name.split('/').collect();
    let (device, filename, legacy) = match components.as_slice() {
        [filename] => (current.as_str(), *filename, false),
        [device, filename] => (*device, *filename, false),
        [device, "legacy", filename] => (*device, *filename, true),
        _ => return Err(AppError::invalid("name", "Invalid backup name")),
    };
    if !file_identity(filename).is_some_and(|(_, owner)| owner == device)
        || Path::new(filename).file_name().and_then(|s| s.to_str()) != Some(filename)
    {
        return Err(AppError::invalid("name", "Invalid backup name"));
    }
    let root = icloud::backup_directory(false)?;
    let path = if legacy {
        root.join(filename)
    } else {
        device_directory(&root, device, false)?.join(filename)
    };
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

/// A failure here precedes the database transaction, leaving the old vault recoverable.
pub fn write_restore_safety(directory: &Path, snapshot: &BackupFile) -> AppResult<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let target = directory.join(format!("before-restore-{}.hatoba", uuid::Uuid::now_v7()));
    write_backup(snapshot, &target)?;
    Ok(target)
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

/// No old backup is deleted while the latest local snapshot lacks upload confirmation.
fn retention_plan(
    mut files: Vec<(i64, PathBuf)>,
    retention: u32,
    latest: Option<&Path>,
    uploaded: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let latest = latest.or_else(|| files.first().map(|(_, path)| path.as_path()));
    let Some(latest) = latest else {
        return Vec::new();
    };
    if !files.iter().any(|(_, path)| path == latest) || !uploaded(latest) {
        return Vec::new();
    }
    let latest = latest.to_path_buf();
    files.sort_by(|a, b| {
        (b.1 == latest)
            .cmp(&(a.1 == latest))
            .then_with(|| b.0.cmp(&a.0))
            .then_with(|| b.1.cmp(&a.1))
    });
    files
        .into_iter()
        .skip(retention.max(1) as usize)
        .map(|(_, path)| path)
        .collect()
}
fn cleanup(state: &AppState, directory: &Path, device: &str, cfg: &Config) -> AppResult<()> {
    let files = snapshots(directory, device)?;
    let latest = cfg.last_name.as_ref().map(|name| directory.join(name));
    let cached = state
        .backups
        .files
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let deletions = retention_plan(files, cfg.retention, latest.as_deref(), |path| {
        cached.get(path).is_some_and(|file| {
            file.transfer == "uploaded"
                && std::fs::symlink_metadata(path)
                    .ok()
                    .is_some_and(|metadata| unchanged_file(&file.metadata, &metadata))
        })
    });
    drop(cached);
    for path in deletions {
        icloud::remove(&path)?;
    }
    Ok(())
}
fn has_last_snapshot(directory: &Path, device: &str, cfg: &Config) -> AppResult<bool> {
    let files = snapshots(directory, device)?;
    Ok(match cfg.last_name.as_ref() {
        Some(name) => files.iter().any(|(_, path)| {
            path.file_name().and_then(|v| v.to_str()).map(visible_name) == Some(name.as_str())
        }),
        None => !files.is_empty(),
    })
}

struct Publication {
    file: BackupFile,
    device: String,
    device_name: String,
    cfg: Config,
}

/// The lifecycle gate covers only the consistent encrypted capture, never file publication.
fn prepare_publication(state: &AppState, automatic: bool) -> AppResult<Option<Publication>> {
    let cfg = config(state)?;
    if automatic && !(cfg.enabled && cfg.automatic) {
        return Ok(None);
    }
    if !cfg.enabled {
        return Err(AppError::invalid("backup", "Enable iCloud Backup first"));
    }
    let _operation = match state.lifecycle_operation() {
        Ok(guard) => guard,
        Err(_) if automatic => return Ok(None),
        Err(error) => return Err(error),
    };
    let vault = state.vault();
    if !vault.is_unlocked() {
        return if automatic {
            Ok(None)
        } else {
            Err(AppError::locked())
        };
    }
    Ok(Some(Publication {
        file: vault.backup_snapshot()?,
        device: vault.device_id(),
        device_name: sealed_device_name(&vault, &vault.device_id())?,
        cfg,
    }))
}

/// A different vault can be installed while immutable ciphertext is being published.
fn record_publication(
    state: &AppState,
    device: &str,
    digest: String,
    at: i64,
    name: String,
) -> AppResult<bool> {
    let mut vault = state.vault();
    if vault.device_id() != device {
        return Ok(false);
    }
    let mut cfg = vault_config(&vault)?;
    cfg.last_at = Some(at);
    cfg.last_digest = Some(digest);
    cfg.last_name = Some(name);
    save_vault_config(&mut vault, &cfg)?;
    Ok(true)
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
    let Some(Publication {
        mut file,
        device,
        device_name,
        cfg,
    }) = prepare_publication(&state, automatic)?
    else {
        return Ok(());
    };
    let owner = device.clone();
    let work_app = app.clone();
    let outcome = blocking(move || {
        let digest = fingerprint(&mut file)?;
        let root = icloud::backup_directory(true)?;
        let directory = device_directory(&root, &device, true)?;
        migrate_legacy(&root, &device, &directory)?;
        if automatic
            && cfg.last_digest.as_deref() == Some(&digest)
            && has_last_snapshot(&directory, &device, &cfg)?
        {
            return Ok(None);
        }
        let temporary = Temporary(staging(&work_app)?);
        write_backup(&file, &temporary.0)?;
        // Structural validation of the bytes written before publishing an immutable snapshot.
        read_backup(&temporary.0)?;
        let profile = directory.join("device.hatoba-device");
        if !profile.try_exists()? {
            let profile_source = Temporary(staging(&work_app)?);
            std::fs::write(&profile_source.0, &device_name)?;
            icloud::publish(&profile_source.0, &profile)?;
        }
        let name = filename(&device, file.created_at);
        let target = directory.join(&name);
        icloud::publish(&temporary.0, &target)?;
        Ok(Some((digest, file.created_at, name, target)))
    })
    .await;
    match outcome {
        Ok(Some((digest, at, name, target))) => {
            if !record_publication(&state, &owner, digest, at, name)? {
                return Ok(());
            }
            *state
                .backups
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            // A new publication starts as pending; even a stale native error cannot flash red.
            if let Ok(metadata) = std::fs::symlink_metadata(&target) {
                let mut file = FileStatus::new(metadata);
                file.checked_at = Some(Instant::now());
                state
                    .backups
                    .files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(target, file);
            }
            emit_status(&app).await;
            start_monitor(app.clone());
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => {
            if state.vault().device_id() != owner {
                return Ok(());
            }
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

pub async fn emit_status(app: &AppHandle) {
    let handle = app.clone();
    if let Ok(status) = blocking(move || status(&handle.state::<AppState>(), false)).await {
        let _ = app.emit("backup://changed", status);
    }
}

/// One bounded watcher for all pending files; status requests share its per-file budget.
/// Nothing polls again after success or the fifth unsuccessful native inspection.
pub fn start_monitor(app: AppHandle) {
    if app
        .state::<AppState>()
        .backups
        .monitoring
        .swap(true, Ordering::AcqRel)
    {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let state = app.state::<AppState>();
            let _gate = state.backups.gate.lock().await;
            let handle = app.clone();
            let result = blocking(move || status(&handle.state::<AppState>(), false)).await;
            let waiting = result.as_ref().is_ok_and(|status| {
                status
                    .devices
                    .iter()
                    .flat_map(|device| &device.entries)
                    .any(|entry| {
                        !["uploaded", "error", "timeout"].contains(&entry.transfer.as_str())
                    })
            });
            if let Ok(status) = result {
                let _ = app.emit("backup://changed", status);
            }
            if !waiting {
                state.backups.monitoring.store(false, Ordering::Release);
                break;
            }
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hatoba-backup-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn owner() -> String {
        uuid::Uuid::now_v7().to_string()
    }
    fn snapshot(directory: &Path, device: &str, at: i64) -> PathBuf {
        let path = directory.join(filename(device, at));
        std::fs::write(&path, b"encrypted-test-fixture").unwrap();
        path
    }
    #[test]
    fn each_device_and_vault_has_its_own_retention_boundary() {
        let root = Directory::new();
        let a = owner();
        let b = owner();
        let another_vault = owner();
        let da = device_directory(&root.0, &a, true).unwrap();
        let db = device_directory(&root.0, &b, true).unwrap();
        let dv = device_directory(&root.0, &another_vault, true).unwrap();
        let a_file = snapshot(&da, &a, 1_700_000_000_000);
        let v_file = snapshot(&dv, &another_vault, 1_700_000_000_001);
        for at in 0..8 {
            snapshot(&db, &b, 1_700_000_000_010 + at);
        }
        let foreign = snapshot(&db, &a, 1_700_000_000_100);
        std::fs::write(root.0.join("notes.txt"), "unrelated").unwrap();
        let files = snapshots(&db, &b).unwrap();
        for path in retention_plan(files, 5, None, |_| true) {
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(snapshots(&db, &b).unwrap().len(), 5);
        for path in retention_plan(snapshots(&db, &b).unwrap(), 1, None, |_| true) {
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(snapshots(&db, &b).unwrap().len(), 1);
        assert!(
            a_file.exists()
                && v_file.exists()
                && foreign.exists()
                && root.0.join("notes.txt").exists()
        );
        assert_eq!(
            device_folders(&root.0).unwrap().len(),
            3,
            "all devices remain visible without shared deletion"
        );
    }
    #[test]
    fn pending_replacement_keeps_previous_uploaded_backup_then_prunes_on_confirmation() {
        let root = Directory::new();
        let id = owner();
        let old = snapshot(&root.0, &id, 1_700_000_000_000);
        let new = snapshot(&root.0, &id, 1_700_000_000_001);
        let files = snapshots(&root.0, &id).unwrap();
        assert!(retention_plan(files.clone(), 1, Some(&new), |path| path == old).is_empty());
        assert_eq!(retention_plan(files, 1, Some(&new), |_| true), vec![old]);
        // Explicit last-publication identity also works if the clock moved backwards.
        assert_eq!(
            retention_plan(snapshots(&root.0, &id).unwrap(), 1, Some(&new), |_| true).len(),
            1
        );
    }
    #[test]
    fn policy_is_saved_before_cleanup_and_write_failure_deletes_nothing() {
        let saved = Cell::new(false);
        let deleted = Cell::new(false);
        let error = save_then_cleanup(
            || Err(AppError::internal("database is read-only")),
            || {
                deleted.set(true);
                Ok(())
            },
        );
        assert!(error.is_err() && !deleted.get());
        let cleanup = save_then_cleanup(
            || {
                saved.set(true);
                Ok(())
            },
            || {
                assert!(saved.get());
                Err(AppError::internal("file coordination failed"))
            },
        )
        .unwrap();
        assert!(saved.get() && cleanup.unwrap().contains("settings saved; cleanup failed"));
    }
    #[test]
    fn legacy_migration_moves_only_owned_files_and_missing_backup_is_recreated() {
        let root = Directory::new();
        let a = owner();
        let b = owner();
        let directory = device_directory(&root.0, &a, true).unwrap();
        let own = snapshot(&root.0, &a, 1_700_000_000_000);
        let other = snapshot(&root.0, &b, 1_700_000_000_001);
        migrate_with(&root.0, &a, &directory, |source, target| {
            Ok(std::fs::rename(source, target)?)
        })
        .unwrap();
        assert!(!own.exists() && other.exists());
        let name = own.file_name().unwrap().to_string_lossy().into_owned();
        let cfg = Config {
            last_name: Some(name.clone()),
            last_digest: Some("unchanged".into()),
            ..Config::default()
        };
        assert!(has_last_snapshot(&directory, &a, &cfg).unwrap());
        std::fs::remove_file(directory.join(name)).unwrap();
        assert!(!has_last_snapshot(&directory, &a, &cfg).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_or_selected_for_retention() {
        let root = Directory::new();
        let id = owner();
        let outside = Directory::new();
        std::os::unix::fs::symlink(&outside.0, root.0.join(&id)).unwrap();
        assert!(device_directory(&root.0, &id, true).is_err());
        assert!(device_folders(&root.0).unwrap().is_empty());
        let name = filename(&id, 1_700_000_000_000);
        std::os::unix::fs::symlink(outside.0.join("anything"), root.0.join(name)).unwrap();
        assert!(snapshots(&root.0, &id).unwrap().is_empty());
    }
    #[test]
    fn five_checks_are_terminal_but_success_on_the_fifth_wins() {
        let root = Directory::new();
        let path = snapshot(&root.0, &owner(), 1_700_000_000_000);
        let mut file = FileStatus::new(std::fs::metadata(&path).unwrap());
        for _ in 0..4 {
            assert_eq!(file.record("error"), "pending");
        }
        assert_eq!(file.record("uploading"), "timeout");
        assert_eq!(file.check(&path, false), "timeout");
        assert_eq!(file.checks, 5);
        let mut file = FileStatus::new(std::fs::metadata(&path).unwrap());
        for _ in 0..4 {
            file.record("pending");
        }
        assert_eq!(file.record("uploaded"), "uploaded");
        assert_eq!(file.check(&path, false), "uploaded");
        assert_eq!(file.checks, 5);
    }
    #[test]
    fn safety_directory_or_write_failure_preserves_the_current_vault() {
        let root = Directory::new();
        let mut vault = hatoba_core::Vault::open_in_memory().unwrap();
        vault
            .create_with_params("restore-safety-test", hatoba_core::KdfParams::for_tests())
            .unwrap();
        let original = vault.device_id();
        let snapshot = vault.backup_snapshot().unwrap();
        let blocked = root.0.join("blocked");
        std::fs::write(&blocked, "file instead of directory").unwrap();
        assert!(write_restore_safety(&blocked, &snapshot).is_err());
        let valid = root.0.join("safety");
        std::fs::create_dir(&valid).unwrap();
        // The core writer's encrypted staging file cannot be created in a missing directory.
        assert!(write_backup(&snapshot, &valid.join("missing/snapshot")).is_err());
        assert_eq!(vault.device_id(), original);
        vault.lock();
        vault.unlock("restore-safety-test").unwrap();
    }
    #[test]
    fn device_names_use_the_sync_envelope_and_foreign_vaults_get_no_plaintext() {
        let root = Directory::new();
        let mut vault = hatoba_core::Vault::open_in_memory().unwrap();
        vault
            .create_with_params("encrypted-device-test", hatoba_core::KdfParams::for_tests())
            .unwrap();
        let device = vault.device_id();
        let sealed = sealed_device_name(&vault, &device).unwrap();
        let json: serde_json::Value = serde_json::from_str(&sealed).unwrap();
        assert!(json.get("c").is_some() && json.get("n").is_some());
        assert!(json.get("name").is_none() && json.get("platform").is_none());
        std::fs::write(root.0.join("device.hatoba-device"), &sealed).unwrap();
        let key = vault.vault_key_copy().unwrap();
        assert_eq!(
            read_device_name(&root.0, &device, Some(&key)).unwrap().name,
            crate::sync::device_info().name
        );
        let wrong = hatoba_core::crypto::random_key().unwrap();
        assert!(read_device_name(&root.0, &device, Some(&wrong)).is_none());
        assert!(read_device_name(&root.0, &owner(), Some(&key)).is_none());
        assert!(read_device_name(&root.0, &device, None).is_none());
    }
    #[test]
    fn evicted_remote_snapshots_remain_visible_without_touching_their_placeholders() {
        let root = Directory::new();
        let device = owner();
        let name = filename(&device, 1_700_000_000_000);
        let placeholder = root.0.join(format!(".{name}.icloud"));
        std::fs::write(&placeholder, "iCloud placeholder fixture").unwrap();
        assert_eq!(
            legacy_devices(&root.0).unwrap(),
            HashSet::from([device.clone()])
        );
        assert!(snapshot_present(&root.0, &name));
        let directory = device_directory(&root.0, &device, true).unwrap();
        migrate_with(&root.0, &device, &directory, |_, _| {
            panic!("do not move an evicted file's placeholder")
        })
        .unwrap();
        assert!(placeholder.exists());
        let mut cache = HashMap::new();
        let mut present = HashSet::new();
        // Seed an uploaded status so the test does not depend on the user's iCloud account.
        let mut file = FileStatus::new(std::fs::metadata(&placeholder).unwrap());
        file.transfer = "uploaded";
        cache.insert(root.0.join(&name), file);
        let entries = read_entries(&root.0, &device, &mut cache, &mut present, false).unwrap();
        assert_eq!(entries[0].name, name);
    }

    #[tokio::test]
    async fn delayed_publication_allows_foreground_changes_and_keeps_a_consistent_snapshot() {
        let mut vault = hatoba_core::Vault::open_in_memory().unwrap();
        vault
            .create_with_params("old-vault-password", hatoba_core::KdfParams::for_tests())
            .unwrap();
        let state = AppState::new(vault, false);
        save_config(
            &state,
            &Config {
                enabled: true,
                automatic: true,
                ..Config::default()
            },
        )
        .unwrap();
        let gate = state.backups.gate.lock().await;
        let publication = prepare_publication(&state, true).unwrap().unwrap();
        // Publication is paused after capture, with the backup gate held as in run().
        let foreground = state.lifecycle_operation().unwrap();
        state
            .vault()
            .change_password("old-vault-password", "new-vault-password")
            .unwrap();
        drop(foreground);
        assert!(state.backups.gate.try_lock().is_err());
        crate::lock::lock_now(&state);
        let directory = Directory::new();
        let path = directory.0.join("snapshot.hatoba");
        write_backup(&publication.file, &path).unwrap();
        read_backup(&path)
            .unwrap()
            .verify("old-vault-password")
            .unwrap();
        assert!(!state.vault().is_unlocked());
        drop(gate);
        state.vault().unlock("new-vault-password").unwrap();
    }

    #[test]
    fn a_publication_from_a_replaced_installation_cannot_change_its_backup_metadata() {
        let mut vault = hatoba_core::Vault::open_in_memory().unwrap();
        vault
            .create_with_params("password", hatoba_core::KdfParams::for_tests())
            .unwrap();
        let state = AppState::new(vault, false);
        save_config(
            &state,
            &Config {
                enabled: true,
                ..Config::default()
            },
        )
        .unwrap();
        let publication = prepare_publication(&state, false).unwrap().unwrap();
        let verified = publication.file.verify("password").unwrap();
        state
            .vault()
            .restore_verified_backup(verified, true)
            .unwrap();
        assert!(
            !record_publication(
                &state,
                &publication.device,
                "old-digest".into(),
                42,
                "old-snapshot.hatoba".into()
            )
            .unwrap()
        );
        assert!(config(&state).unwrap().last_at.is_none());
    }
}
