//! Update orchestrator — coordinates download → verify → prepare → restart.
//!
//! High-level API consumed by `WindowManager`. All blocking work runs on
//! background threads; progress is communicated via `Arc<Mutex<UpdatePhase>>`.

use std::path::Path;

use super::update::{UpdateChannel, UpdateChecker, UpdateInfo, UpdatePhase};

/// Resolve the current package when the user clicks download. Keep its URL,
/// filename and checksum together rather than redirecting a stale package URL.
pub fn download_latest_and_prepare(
    channel: UpdateChannel,
    phase_callback: impl Fn(UpdatePhase) + Send + 'static,
    on_resolved: impl FnOnce(UpdateInfo),
) -> Result<(), String> {
    phase_callback(UpdatePhase::Checking);
    let checker = UpdateChecker::new(env!("CARGO_PKG_VERSION"), "Ruszero01", "clippi");
    let latest = resolve_download_info(
        || {
            checker
                .check_full(channel)
                .map_err(|error| error.user_message())
        },
        on_resolved,
    )?;
    match latest {
        Some(info) => download_and_prepare(&info, phase_callback),
        None => {
            phase_callback(UpdatePhase::UpToDate);
            Ok(())
        }
    }
}

fn resolve_download_info(
    resolve: impl FnOnce() -> Result<Option<UpdateInfo>, String>,
    on_resolved: impl FnOnce(UpdateInfo),
) -> Result<Option<UpdateInfo>, String> {
    let info = resolve()?;
    if let Some(info) = &info {
        on_resolved(info.clone());
    }
    Ok(info)
}

/// Download, verify, and prepare an update. Blocking — call from background thread.
/// `phase_callback` is invoked on each phase transition for UI updates.
fn download_and_prepare(
    info: &UpdateInfo,
    phase_callback: impl Fn(UpdatePhase) + Send + 'static,
) -> Result<(), String> {
    // Clean up previous downloads so only the latest installer is kept.
    cleanup_temp();
    let temp_dir = super::install::update_temp_dir();
    let _ = std::fs::create_dir_all(&temp_dir);

    // The asset name comes from an external manifest/release. Reject anything
    // that is not a plain file name before joining it onto the temp directory.
    if !super::update::is_safe_asset_name(&info.asset_name) {
        return Err(format!("Invalid update asset name: {}", info.asset_name));
    }
    let dest_path = temp_dir.join(&info.asset_name);

    // 1. Download
    phase_callback(UpdatePhase::Downloading { progress: 0 });

    super::downloader::download_file(
        &info.download_url,
        &dest_path,
        info.asset_size,
        |progress| {
            phase_callback(UpdatePhase::Downloading { progress });
        },
    )?;

    // 2. Verify. The OSS manifest embeds the hash, which avoids a second
    // request and any window where the checksum file and installer disagree.
    phase_callback(UpdatePhase::Verifying);

    let expected_hash = match info.sha256.as_deref() {
        Some(hash) => hash.to_string(),
        None => super::downloader::fetch_checksum(&info.checksum_url)?,
    };
    super::downloader::verify_sha256(&dest_path, &expected_hash)?;

    // 3. Prepare the platform artifact. Installation happens only after the
    // user clicks restart, so the running executable is never replaced here.
    phase_callback(UpdatePhase::Installing);
    prepare_asset(&dest_path, &temp_dir)?;

    phase_callback(UpdatePhase::ReadyToRestart);
    Ok(())
}

/// Prepare the downloaded asset for installation after the app exits.
fn prepare_asset(asset_path: &Path, temp_dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let _ = (asset_path, temp_dir);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        super::install::prepare_macos_app(asset_path, temp_dir).map(|_| ())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = asset_path;
        Err("Platform not supported".into())
    }
}

/// Launch the prepared platform installer.
///
/// On Windows this starts the NSIS installer in silent mode. On macOS the
/// caller must quit after this returns successfully so the helper can replace
/// the application bundle.
pub fn launch_prepared_update(info: &UpdateInfo, parent_hwnd: isize) -> Result<(), String> {
    let temp_dir = super::install::update_temp_dir();
    #[cfg(target_os = "windows")]
    {
        super::install::launch_nsis_installer(&temp_dir.join(&info.asset_name), parent_hwnd)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (info, parent_hwnd);
        super::install::launch_macos_installer(&temp_dir.join("Clippi.app"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (info, temp_dir, parent_hwnd);
        Err("Platform not supported".into())
    }
}

/// Clean up old temp files from previous updates. Call at startup.
pub fn cleanup_temp() {
    let temp_dir = super::install::update_temp_dir();
    #[cfg(target_os = "macos")]
    {
        let mount_point = temp_dir.join("mount");
        if mount_point.exists() {
            let _ = std::process::Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&mount_point)
                .status();
        }
    }
    if temp_dir.exists() && std::fs::remove_dir_all(&temp_dir).is_err() {
        #[cfg(target_os = "windows")]
        {
            terminate_processes_in_dir(&temp_dir);
            let _ = std::fs::remove_dir_all(&temp_dir);
        }
    }
}

#[cfg(target_os = "windows")]
fn terminate_processes_in_dir(dir: &Path) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE,
    };

    let Ok(dir) = std::fs::canonicalize(dir) else {
        return;
    };

    let mut pids = vec![0u32; 2048];
    let mut bytes_needed = 0u32;
    let ok = unsafe {
        EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * std::mem::size_of::<u32>()) as u32,
            &mut bytes_needed,
        )
    };
    if ok == 0 {
        return;
    }

    let count = (bytes_needed as usize) / std::mem::size_of::<u32>();
    for pid in pids.into_iter().take(count).filter(|pid| *pid != 0) {
        // SAFETY: Process handles are opened with the minimum rights needed to
        // query, terminate, and wait. Every non-null handle is closed exactly
        // once. We only terminate executables whose canonical path is inside
        // Clippi's update temp directory.
        unsafe {
            let process = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            );
            if process.is_null() {
                continue;
            }

            let mut buf = [0u16; 32768];
            let mut len = buf.len() as u32;
            let queried =
                QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
            if queried != 0 {
                let exe = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
                if exe.starts_with(&dir) {
                    log::warn!(
                        "cleanup_temp: terminating stale update process {} ({})",
                        pid,
                        exe.display()
                    );
                    if TerminateProcess(process, 1) != 0 {
                        let _ = WaitForSingleObject(process, 3000);
                    }
                }
            }
            CloseHandle(process);
        }
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;
    use crate::services::update::UpdateSource;

    #[test]
    fn resolved_package_updates_installer_name_and_checksum_together() {
        let latest = UpdateInfo {
            latest_version: "0.5.1".into(),
            release_notes: "Latest release".into(),
            download_url: "https://example.invalid/v0.5.1/Clippi_0.5.1_x64-setup.exe".into(),
            checksum_url: String::new(),
            asset_name: "Clippi_0.5.1_x64-setup.exe".into(),
            asset_size: 123,
            sha256: Some("b".repeat(64)),
            source: UpdateSource::Oss,
        };
        let mut published = None;
        let download = resolve_download_info(|| Ok(Some(latest)), |info| published = Some(info))
            .unwrap()
            .unwrap();
        let installer = published.unwrap();
        assert_eq!(download.latest_version, "0.5.1");
        assert_eq!(download.asset_name, "Clippi_0.5.1_x64-setup.exe");
        assert_eq!(download.sha256, Some("b".repeat(64)));
        assert_eq!(installer.download_url, download.download_url);
        assert_eq!(installer.asset_name, download.asset_name);
        assert_eq!(installer.asset_size, download.asset_size);
        assert_eq!(installer.sha256, download.sha256);
    }

    #[test]
    fn no_latest_package_or_failed_check_never_reuses_the_old_prompt() {
        assert!(
            resolve_download_info(|| Ok(None), |_| panic!("no package to install"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve_download_info(|| Err("offline".into()), |_| panic!("check failed"))
                .err()
                .unwrap(),
            "offline"
        );
    }
}
