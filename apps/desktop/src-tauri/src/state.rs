//! Process-wide state managed by Tauri. Secrets only ever live here, in Rust (spec §3.2).

use std::sync::{Arc, Mutex, MutexGuard};

use std::time::Instant;

use hatoba_core::vault::Vault;
use tauri::{AppHandle, Manager};

use crate::ai::AiManager;
use crate::deploy::Deployments;
use crate::dropped::DroppedPaths;
use crate::error::{AppError, AppResult};
use crate::lock::LockPolicy;
use crate::mcp::McpManager;
use crate::platform::secrets::KeyringStore;
use crate::ssh::SshManager;
use crate::sync::SyncController;
use crate::update::Updates;

pub use hatoba_core::sync::SharedVault;

pub struct AppState {
    pub vault: SharedVault,
    pub backups: crate::backup::Backups,
    /// Serializes key and connection changes with restore, including awaited work.
    pub lifecycle: tokio::sync::Mutex<()>,
    pub secrets: Arc<KeyringStore>,
    pub ssh: SshManager,
    pub sync: SyncController,
    /// The in-app deployment in progress (§6.7).
    pub deploy: Deployments,
    /// The AI assistant's running turns and tools (§13.1).
    pub ai: AiManager,
    /// The MCP servers' connections and live state (§13.9); `ai` offers their tools.
    pub mcp: McpManager,
    /// The paths of the window's last file drop, which the AI panel may read (AI-35).
    pub dropped: DroppedPaths,
    /// The update the last check found (spec §11).
    pub updates: Updates,
    pub mica: bool,
    pub lock_policy: LockPolicy,
    last_activity: Mutex<Instant>,
}

impl AppState {
    pub fn new(vault: Vault, mica: bool) -> Self {
        let mcp = McpManager::default();
        Self {
            vault: Arc::new(Mutex::new(vault)),
            backups: crate::backup::Backups::default(),
            lifecycle: tokio::sync::Mutex::new(()),
            secrets: Arc::new(KeyringStore),
            ssh: SshManager::default(),
            sync: SyncController::default(),
            deploy: Deployments::default(),
            ai: AiManager::with_mcp(mcp.clone()),
            mcp,
            dropped: DroppedPaths::default(),
            updates: Updates::default(),
            mica,
            lock_policy: LockPolicy::default(),
            last_activity: Mutex::new(Instant::now()),
        }
    }

    /// Rejects concurrent lifecycle requests rather than queueing an old password/key request.
    pub fn lifecycle_operation(&self) -> AppResult<tokio::sync::MutexGuard<'_, ()>> {
        self.lifecycle
            .try_lock()
            .map_err(|_| AppError::invalid("vault", "A vault operation is already in progress"))
    }

    /// Locks the vault mutex. Poisoning only happens after a panic while holding it; the vault's
    /// own invariants are transactional, so recovering the guard is safe.
    pub fn vault(&self) -> MutexGuard<'_, Vault> {
        self.vault.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs `f` with the vault, requiring it to be unlocked.
    pub fn with_unlocked<R>(&self, f: impl FnOnce(&mut Vault) -> AppResult<R>) -> AppResult<R> {
        let mut vault = self.vault();
        if !vault.is_unlocked() {
            return Err(AppError::locked());
        }
        f(&mut vault)
    }

    pub fn touch_activity(&self) {
        *self.last_activity.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    pub fn idle_for(&self) -> std::time::Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed()
    }
}

pub fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Runs CPU-heavy work (Argon2id, RSA generation) off the async runtime.
pub async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> AppResult<R> + Send + 'static,
) -> AppResult<R> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(format!("background task failed: {e}")))?
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[tokio::test]
    async fn restore_excludes_unlock_and_connection_changes_through_awaited_safety_work() {
        let mut vault = Vault::open_in_memory().unwrap();
        let recovery = vault
            .create_with_params(
                "original-vault-password",
                hatoba_core::KdfParams::for_tests(),
            )
            .unwrap()
            .to_string();
        let original_id = vault.device_id();
        let old_key = vault.vault_key_copy().unwrap();
        let state = Arc::new(AppState::new(vault, false));
        let restore = state.lifecycle_operation().unwrap();
        state.vault().lock();
        // Restore is paused here while its encrypted safety write awaits. Each request would
        // unlock the old vault and call after_unlock if the lifecycle guard were absent.
        let password = {
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = state.lifecycle_operation()?;
                state.vault().unlock("original-vault-password")?;
                Ok::<_, AppError>(())
            })
        };
        let biometric = {
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = state.lifecycle_operation()?;
                state.vault().unlock_with_vault_key(old_key)?;
                Ok::<_, AppError>(())
            })
        };
        let recovery = {
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = state.lifecycle_operation()?;
                state.vault().recover(&recovery, "replacement-password")?;
                Ok::<_, AppError>(())
            })
        };
        assert!(password.await.unwrap().is_err());
        assert!(biometric.await.unwrap().is_err());
        assert!(recovery.await.unwrap().is_err());
        assert!(!state.vault().is_unlocked());
        assert_eq!(state.vault().device_id(), original_id);
        drop(restore); // The safety write failed: recovery using the original password is possible.
        let _guard = state.lifecycle_operation().unwrap();
        state.vault().unlock("original-vault-password").unwrap();
        assert!(state.vault().is_unlocked());
    }
}
