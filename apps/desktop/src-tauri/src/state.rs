//! Process-wide state managed by Tauri. Secrets only ever live here, in Rust (spec §3.2).

use std::sync::{Arc, Mutex, MutexGuard};

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Lock requests bypass this gate so prompts and network work cannot delay locking.
    lifecycle: tokio::sync::Mutex<()>,
    lifecycle_busy: AtomicBool,
    lock_cleanup: AtomicBool,
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
            lifecycle_busy: AtomicBool::new(false),
            lock_cleanup: AtomicBool::new(false),
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
    pub fn lifecycle_operation(&self) -> AppResult<LifecycleOperation<'_>> {
        self.lifecycle
            .try_lock()
            .map(|guard| self.track_lifecycle(guard))
            .map_err(|_| AppError::invalid("vault", "A vault operation is already in progress"))
    }

    /// Observation never claims the mutex or competes with a foreground request.
    pub(crate) fn lifecycle_busy(&self) -> bool {
        self.lifecycle_busy.load(Ordering::Acquire)
    }

    fn track_lifecycle<'a>(
        &'a self,
        guard: tokio::sync::MutexGuard<'a, ()>,
    ) -> LifecycleOperation<'a> {
        self.lifecycle_busy.store(true, Ordering::Release);
        LifecycleOperation {
            _guard: guard,
            busy: &self.lifecycle_busy,
        }
    }

    /// Cleanup still excludes new sessions and MCP servers, but unlock can visibly wait for it.
    pub(crate) fn lock_cleanup_operation(&self) -> Option<LockCleanup<'_>> {
        let guard = self.lifecycle_operation().ok()?;
        self.lock_cleanup.store(true, Ordering::Release);
        Some(LockCleanup {
            _guard: guard,
            cleanup: &self.lock_cleanup,
        })
    }

    /// Queue only behind lock cleanup, never behind a restore or other key replacement.
    pub async fn unlock_operation(&self) -> AppResult<LifecycleOperation<'_>> {
        let generation = self.vault().lock_generation();
        let guard = match self.lifecycle.try_lock() {
            Ok(guard) => guard,
            Err(_) if self.lock_cleanup.load(Ordering::Acquire) => self.lifecycle.lock().await,
            Err(_) => return self.lifecycle_operation(),
        };
        let operation = self.track_lifecycle(guard);
        self.vault().ensure_lock_generation(generation)?;
        Ok(operation)
    }

    /// Locks the vault mutex. Poisoning only happens after a panic while holding it; the vault's
    /// own invariants are transactional, so recovering the guard is safe.
    pub fn vault(&self) -> MutexGuard<'_, Vault> {
        self.vault.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Checks and uses an operation's lock generation under the same vault mutex.
    pub fn with_lock_generation<R>(
        &self,
        expected: u64,
        f: impl FnOnce(&mut Vault) -> AppResult<R>,
    ) -> AppResult<R> {
        let mut vault = self.vault();
        vault.ensure_lock_generation(expected)?;
        f(&mut vault)
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

/// Every lifecycle entry point owns the busy marker until its operation ends.
#[derive(Debug)]
#[must_use = "Hold this guard for the complete lifecycle operation"]
pub struct LifecycleOperation<'a> {
    _guard: tokio::sync::MutexGuard<'a, ()>,
    busy: &'a AtomicBool,
}
impl Drop for LifecycleOperation<'_> {
    fn drop(&mut self) {
        // Clear before releasing the mutex, so a new owner's marker cannot be overwritten.
        self.busy.store(false, Ordering::Release);
    }
}

/// Marks the lifecycle guard held only for post-lock session cleanup.
pub(crate) struct LockCleanup<'a> {
    _guard: LifecycleOperation<'a>,
    cleanup: &'a AtomicBool,
}
impl Drop for LockCleanup<'_> {
    fn drop(&mut self) {
        self.cleanup.store(false, Ordering::Release);
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

    #[tokio::test]
    async fn an_unlock_waits_for_delayed_cleanup_without_becoming_a_credential_error() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        state
            .vault()
            .create_with_params("test-password", hatoba_core::KdfParams::for_tests())
            .unwrap();
        let cleanup = state.lock_cleanup_operation().unwrap();
        assert!(state.lifecycle_busy());
        crate::lock::lock_now(&state);
        let mut request = Box::pin(state.unlock_operation());
        assert!(futures::poll!(request.as_mut()).is_pending());
        assert!(!state.vault().is_unlocked());
        drop(cleanup); // Represents awaited MCP/session shutdown finishing.
        assert!(!state.lifecycle_busy());
        let operation = request.await.unwrap();
        assert!(state.lifecycle_busy());
        state.vault().unlock("test-password").unwrap();
        assert!(state.vault().is_unlocked());
        drop(operation);
        assert!(!state.lifecycle_busy());
    }

    #[tokio::test]
    async fn a_new_lock_invalidates_an_unlock_waiting_for_cleanup() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        let cleanup = state.lock_cleanup_operation().unwrap();
        crate::lock::lock_now(&state);
        let mut request = Box::pin(state.unlock_operation());
        assert!(futures::poll!(request.as_mut()).is_pending());
        crate::lock::lock_now(&state); // Sleep/manual lock while the request is queued.
        drop(cleanup);
        assert_eq!(
            request.await.unwrap_err().code,
            crate::error::ErrorCode::Locked
        );
        assert!(!state.vault().is_unlocked());
        assert!(!state.lifecycle_busy());
    }

    #[tokio::test]
    async fn unlock_does_not_queue_an_old_password_behind_restore() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        let _restore = state.lifecycle_operation().unwrap();
        assert!(state.unlock_operation().await.is_err());
    }

    #[test]
    fn busy_marker_follows_the_owner_and_failed_requests_cannot_clear_it() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        assert!(!state.lifecycle_busy());
        let operation = state.lifecycle_operation().unwrap();
        assert!(state.lifecycle_busy());
        assert!(state.lifecycle_operation().is_err());
        assert!(state.lock_cleanup_operation().is_none());
        assert!(state.lifecycle_busy());
        drop(operation);
        assert!(!state.lifecycle_busy());
        let cleanup = state.lock_cleanup_operation().unwrap();
        assert!(state.lifecycle_busy());
        drop(cleanup);
        assert!(!state.lifecycle_busy());
    }

    #[tokio::test]
    async fn observing_busy_does_not_probe_a_mutex_reserved_for_a_waiter() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        let raw = state.lifecycle.try_lock().unwrap();
        let mut waiter = Box::pin(state.lifecycle.lock());
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        drop(raw); // Tokio reserves the permit for the queued request, not an active operation.
        assert!(state.lifecycle.try_lock().is_err());
        assert!(!state.lifecycle_busy());
        let operation = state.track_lifecycle(waiter.await);
        assert!(state.lifecycle_busy());
        drop(operation);
        assert!(!state.lifecycle_busy());
    }

    #[tokio::test]
    async fn cancelling_a_waiting_unlock_preserves_the_cleanup_owners_busy_marker() {
        let state = AppState::new(Vault::open_in_memory().unwrap(), false);
        let cleanup = state.lock_cleanup_operation().unwrap();
        let mut request = Box::pin(state.unlock_operation());
        assert!(futures::poll!(request.as_mut()).is_pending());
        drop(request);
        assert!(state.lifecycle_busy());
        drop(cleanup);
        assert!(!state.lifecycle_busy());
    }
}
