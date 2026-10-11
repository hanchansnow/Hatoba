//! macOS iCloud Drive file coordination; no dedicated app container is used.
use crate::error::{AppError, AppResult};
use std::path::Path;

#[derive(serde::Deserialize)]
pub struct CloudState {
    #[serde(default)]
    pub ubiquitous: bool,
    #[serde(default)]
    pub uploading: bool,
    #[serde(default)]
    pub uploaded: bool,
    #[serde(default)]
    pub upload_error: bool,
}

impl CloudState {
    pub fn transfer(&self) -> &'static str {
        // Active retries and completed uploads take precedence over a previous upload error.
        if self.uploading {
            "uploading"
        } else if self.uploaded {
            "uploaded"
        } else if self.upload_error {
            "error"
        } else if self.ubiquitous {
            "pending"
        } else {
            "unknown"
        }
    }
}

#[cfg(target_os = "macos")]
fn call(operation: &str, path: &Path, other: Option<&Path>) -> AppResult<serde_json::Value> {
    use std::ffi::{CStr, CString, c_char};
    unsafe extern "C" {
        fn hatoba_icloud(input: *const c_char) -> *mut c_char;
        fn hatoba_icloud_free(output: *mut c_char);
    }
    let request = CString::new(
        serde_json::json!({ "operation": operation, "path": path,
        "other": other })
        .to_string(),
    )
    .map_err(|_| AppError::invalid("path", "Invalid file path"))?;
    // The Foundation bridge copies the input, and returns one malloc-owned UTF-8 JSON string.
    let value = unsafe {
        let output = hatoba_icloud(request.as_ptr());
        if output.is_null() {
            return Err(AppError::internal("iCloud bridge allocation failed"));
        }
        let parsed = serde_json::from_slice::<serde_json::Value>(CStr::from_ptr(output).to_bytes());
        hatoba_icloud_free(output);
        parsed.map_err(|_| AppError::internal("Invalid iCloud bridge response"))?
    };
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(AppError::io(std::io::Error::other(error)));
    }
    Ok(value)
}

#[cfg(not(target_os = "macos"))]
fn call(_: &str, _: &Path, _: Option<&Path>) -> AppResult<serde_json::Value> {
    Err(AppError::invalid(
        "platform",
        "iCloud backup is available on macOS only",
    ))
}

pub fn inspect(path: &Path) -> AppResult<CloudState> {
    serde_json::from_value(call("inspect", path, None)?)
        .map_err(|_| AppError::internal("Invalid iCloud file status"))
}
pub fn publish(source: &Path, target: &Path) -> AppResult<()> {
    call("publish", target, Some(source)).map(|_| ())
}
pub fn move_file(source: &Path, target: &Path) -> AppResult<()> {
    call("move", source, Some(target)).map(|_| ())
}
pub fn remove(path: &Path) -> AppResult<()> {
    call("remove", path, None).map(|_| ())
}
pub fn read(source: &Path, target: &Path) -> AppResult<()> {
    call("download", source, None)?;
    let metadata = std::fs::metadata(source)?;
    if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
        return Err(AppError::invalid(
            "path",
            "Select a backup file smaller than 256 MiB",
        ));
    }
    call("read", source, Some(target)).map(|_| ())
}

/// The fixed Hatoba folder in the user's existing iCloud Drive.
/// Never create a local CloudDocs root when iCloud Drive is unavailable.
pub fn backup_directory(create: bool) -> AppResult<std::path::PathBuf> {
    if !cfg!(target_os = "macos") {
        return Err(AppError::invalid("platform", "macOS only"));
    }
    let drive = dirs::home_dir()
        .ok_or_else(|| AppError::internal("Home directory unavailable"))?
        .join("Library/Mobile Documents/com~apple~CloudDocs");
    fixed_directory(&drive, create)
}
fn fixed_directory(drive: &Path, create: bool) -> AppResult<std::path::PathBuf> {
    let root = drive.canonicalize()?;
    if !root.is_dir() {
        return Err(AppError::invalid("backup", "iCloud Drive is unavailable"));
    }
    let folder = root.join("Hatoba");
    if create && !folder.exists() {
        match std::fs::create_dir(&folder) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = std::fs::symlink_metadata(&folder)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::invalid(
            "backup",
            "Hatoba must be a folder in iCloud Drive",
        ));
    }
    let canonical = folder.canonicalize()?;
    if canonical != folder {
        return Err(AppError::invalid("backup", "Invalid iCloud backup folder"));
    }
    Ok(canonical)
}

#[cfg(all(test, target_os = "macos"))]
mod download_tests {
    use super::*;
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("hatoba-native-download-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn native_download_handles_unknown_status_failure_materialization_and_timeout() {
        let directory = Directory::new();
        let binary = directory.0.join("download-tests");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform/icloud_tests.m");
        let compiled = std::process::Command::new("xcrun")
            .args(["clang", "-fobjc-arc", "-framework", "Foundation"])
            .arg(source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let output = std::process::Command::new(binary).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_actual_bridge_rejects_missing_files_and_accepts_readable_local_files() {
        let directory = Directory::new();
        let source = directory.0.join("snapshot.hatoba-backup");
        let target = directory.0.join("staged.hatoba-backup");
        assert!(call("download", &source, None).is_err());
        assert!(read(&source, &target).is_err());
        assert!(!target.exists());
        std::fs::write(&source, b"encrypted fixture bytes").unwrap();
        read(&source, &target).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"encrypted fixture bytes");
    }
}
