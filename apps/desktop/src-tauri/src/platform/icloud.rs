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
