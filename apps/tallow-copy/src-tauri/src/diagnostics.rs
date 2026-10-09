use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendDiagnosticEntry {
    pub level: String,
    pub message: String,
    pub context: Option<String>,
}

/// The per-user data directory this platform actually uses, or `None` when the environment says
/// nothing about one.
///
/// This used to read `LOCALAPPDATA` and fall back to the temp directory, which is right on Windows
/// and wrong everywhere else: on Linux every log landed in `/tmp` and died with the next reboot, so
/// a user reporting a problem had nothing to attach. The app builds and runs on Linux, so the path
/// is now platform-specific. `None` still means "fall back to the temp directory" - a transfer tool
/// that refuses to log is worse than one whose log is in temp.
pub(crate) fn data_root() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
            })
    } else {
        // XDG_DATA_HOME when it is set (that is the spec's answer), else the spec's default.
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local").join("share"))
            })
    }
}

/// Split from `diagnostics_dir` so the platform rule and the fallback can be tested without
/// mutating the process environment, which is global and would race other tests.
fn diagnostics_dir_under(root: Option<PathBuf>) -> PathBuf {
    root.unwrap_or_else(std::env::temp_dir)
        .join("Tallow Copy")
        .join("logs")
}

pub fn diagnostics_dir() -> PathBuf {
    diagnostics_dir_under(data_root())
}

pub fn diagnostics_log_path() -> PathBuf {
    diagnostics_dir().join("tallow-copy.log")
}

/// How this platform is asked to show a file. Windows' explorer selects it in its folder; macOS
/// reveals it in Finder; there is no standard "select" on Linux, so the folder that holds it is
/// opened. Split out so the invocation is testable without launching anything.
fn reveal_invocation(path: &Path) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "windows") {
        (
            "explorer.exe",
            vec![format!("/select,{}", path.to_string_lossy())],
        )
    } else if cfg!(target_os = "macos") {
        (
            "open",
            vec!["-R".to_string(), path.to_string_lossy().to_string()],
        )
    } else {
        let directory = path.parent().unwrap_or(path);
        ("xdg-open", vec![directory.to_string_lossy().to_string()])
    }
}

pub fn log_info(message: impl AsRef<str>) {
    append_line("INFO", "backend", message.as_ref());
}

pub fn log_warn(message: impl AsRef<str>) {
    append_line("WARN", "backend", message.as_ref());
}

pub fn log_error(message: impl AsRef<str>) {
    append_line("ERROR", "backend", message.as_ref());
}

pub fn log_frontend(entry: FrontendDiagnosticEntry) {
    let context = entry
        .context
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" | {value}"))
        .unwrap_or_default();
    append_line(
        entry.level.to_uppercase(),
        "frontend",
        &format!("{}{}", entry.message, context),
    );
}

fn append_line(level: impl AsRef<str>, source: &str, message: &str) {
    let path = diagnostics_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format!("{}.{:03}", duration.as_secs(), duration.subsec_millis()))
        .unwrap_or_else(|_| "time-error".to_string());
    let thread_id = format!("{:?}", std::thread::current().id());
    let line = format!(
        "{timestamp} [{}] pid={} thread={thread_id} source={source} {message}\n",
        level.as_ref(),
        std::process::id(),
    );

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(not(test))]
#[tauri::command]
pub fn get_diagnostics_log_path() -> String {
    diagnostics_log_path().to_string_lossy().to_string()
}

#[cfg(not(test))]
#[tauri::command]
pub fn write_diagnostic_log(entry: FrontendDiagnosticEntry) {
    log_frontend(entry);
}

#[cfg(not(test))]
#[tauri::command]
pub fn reveal_diagnostics_log() -> Result<(), String> {
    let path = diagnostics_log_path();
    if !path.exists() {
        log_info("created diagnostics log on reveal request");
    }

    let (program, args) = reveal_invocation(&path);
    let status = std::process::Command::new(program)
        .args(&args)
        .status()
        .map_err(|err| format!("failed to launch {program}: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_sits_under_the_platform_data_root() {
        assert_eq!(
            diagnostics_dir_under(Some(PathBuf::from("/data"))),
            PathBuf::from("/data/Tallow Copy/logs")
        );
    }

    #[test]
    fn a_missing_data_root_falls_back_to_the_temp_directory() {
        let directory = diagnostics_dir_under(None);
        assert!(
            directory.starts_with(std::env::temp_dir()),
            "{}",
            directory.display()
        );
        assert!(
            directory.ends_with("Tallow Copy/logs"),
            "{}",
            directory.display()
        );
    }

    #[test]
    fn on_a_platform_with_a_data_root_the_log_is_not_left_in_temp() {
        // The regression this guards: `LOCALAPPDATA` is Windows-only, so every Linux log landed in
        // the temp directory and died with the reboot. Where the environment genuinely has no data
        // root the fallback above is the documented behaviour, so the assertion is skipped rather
        // than failed.
        if data_root().is_none() {
            return;
        }
        let directory = diagnostics_dir();
        assert!(
            !directory.starts_with(std::env::temp_dir()),
            "the log must not live in the temp directory when the platform has a data root: {}",
            directory.display()
        );
    }

    #[test]
    fn the_reveal_invocation_matches_the_platform() {
        let path = Path::new("/tmp/Tallow Copy/logs/tallow-copy.log");
        let (program, args) = reveal_invocation(path);
        if cfg!(target_os = "windows") {
            assert_eq!(program, "explorer.exe");
            assert!(args[0].starts_with("/select,"), "{args:?}");
        } else if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
            assert_eq!(args[0], "-R");
        } else {
            assert_eq!(program, "xdg-open");
            assert!(
                args[0].ends_with("logs"),
                "on Linux the folder holding the log is what opens: {args:?}"
            );
        }
    }
}
