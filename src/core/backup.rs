//! Selective local backups. Restore data into a new directory before switching settings;
//! the live database and the original data are never overwritten.
use super::settings::AppSettings;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub const PLATFORM_MISMATCH: &str = "Backup platform does not match this operating system";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupContents {
    pub settings: bool,
    pub data: bool,
}

impl Default for BackupContents {
    fn default() -> Self {
        Self {
            settings: true,
            data: true,
        }
    }
}

impl BackupContents {
    pub fn is_empty(self) -> bool {
        !self.settings && !self.data
    }
    pub fn count(self) -> usize {
        usize::from(self.settings) + usize::from(self.data)
    }
    fn validate_selection(self, available: Self) -> Result<(), String> {
        if self.is_empty()
            || (self.settings && !available.settings)
            || (self.data && !available.data)
        {
            Err("Select at least one available backup component".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    #[serde(default)]
    platform: String,
    data_dir: PathBuf,
    files: BTreeMap<String, String>,
    #[serde(default)]
    contents: BackupContents,
}

fn zip_tree(
    root: &Path,
    directory: &Path,
    archive: &mut zip::ZipWriter<fs::File>,
) -> Result<(), String> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("Backup filename is not valid UTF-8")?
            .replace('\\', "/");
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            archive
                .add_directory(format!("{name}/"), options)
                .map_err(|e| e.to_string())?;
            zip_tree(root, &path, archive)?;
        } else {
            archive
                .start_file(name, options)
                .map_err(|e| e.to_string())?;
            let mut file = fs::File::open(&path).map_err(|e| e.to_string())?;
            std::io::copy(&mut file, archive).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Write one ZIP backup, replacing an existing destination only after success.
pub fn create_selected(
    settings: &AppSettings,
    destination: &Path,
    contents: BackupContents,
) -> Result<(), String> {
    contents.validate_selection(BackupContents::default())?;
    let parent = destination
        .parent()
        .ok_or("Backup has no parent directory")?;
    let parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    let db = settings.resolve_db_path();
    if let Ok(existing) = fs::canonicalize(destination) {
        for live in [db.clone(), super::paths::config_path()] {
            if fs::canonicalize(live).ok().as_ref() == Some(&existing) {
                return Err("Cannot overwrite the active database or settings".into());
            }
        }
    }
    let data = fs::canonicalize(db.parent().ok_or("Database has no parent directory")?)
        .map_err(|e| e.to_string())?;
    if ["images", "file_cache"]
        .iter()
        .any(|cache| parent.starts_with(data.join(cache)))
    {
        return Err("Choose a backup location outside the image/file cache".into());
    }
    let staging = tempfile::tempdir().map_err(|e| e.to_string())?;
    let snapshot = create_folder_selected(settings, staging.path(), contents)?;
    let temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipWriter::new(temporary.reopen().map_err(|e| e.to_string())?);
    zip_tree(&snapshot, &snapshot, &mut archive)?;
    let mut file = archive.finish().map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    let temporary = temporary.into_temp_path();
    crate::services::file_ops::replace_file(&temporary, destination).map_err(|e| e.to_string())
}

fn extract(source: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(source).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut names = std::collections::HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let path = entry.enclosed_name().ok_or("Unsafe backup archive path")?;
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || entry.name().contains('\\')
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || !names.insert(entry.name().trim_end_matches('/').to_lowercase())
        {
            return Err("Unsafe or duplicate backup archive entry".into());
        }
        let target = destination.join(path);
        if entry.is_dir() {
            fs::create_dir_all(target).map_err(|e| e.to_string())?;
        } else {
            fs::create_dir_all(target.parent().ok_or("Archive entry has no parent")?)
                .map_err(|e| e.to_string())?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// An extracted, validated backup kept alive while the user chooses contents.
pub struct PreparedRestore {
    staging: tempfile::TempDir,
    contents: BackupContents,
}

impl PreparedRestore {
    pub fn contents(&self) -> BackupContents {
        self.contents
    }
    pub fn restore(
        &self,
        current: &AppSettings,
        selected: BackupContents,
    ) -> Result<AppSettings, String> {
        restore_folder_selected(self.staging.path(), current, selected)
    }
}

pub fn prepare_restore(source: &Path) -> Result<PreparedRestore, String> {
    let staging = tempfile::tempdir().map_err(|e| e.to_string())?;
    extract(source, staging.path())?;
    let manifest = validate_snapshot(staging.path())?;
    Ok(PreparedRestore {
        staging,
        contents: manifest.contents,
    })
}

#[cfg(test)]
fn create(settings: &AppSettings, destination: &Path) -> Result<(), String> {
    create_selected(settings, destination, BackupContents::default())
}

#[cfg(test)]
fn restore(source: &Path, current: &AppSettings) -> Result<AppSettings, String> {
    let prepared = prepare_restore(source)?;
    prepared.restore(current, prepared.contents())
}

#[cfg(test)]
fn create_folder(settings: &AppSettings, parent: &Path) -> Result<PathBuf, String> {
    create_folder_selected(settings, parent, BackupContents::default())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            return Err(format!(
                "Unsupported backup entry: {}",
                entry.path().display()
            ));
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), target.join(entry.file_name())).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn inventory(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            return Err(format!("Unsupported backup entry: {}", path.display()));
        }
        if kind.is_dir() {
            inventory(root, &path, files)?;
        } else if path != root.join("manifest.json") {
            let mut file = fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            files.insert(
                path.strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
                format!("{:x}", hash.finalize()),
            );
        }
    }
    Ok(())
}

/// Create a private snapshot folder to package into an archive.
fn create_folder_selected(
    settings: &AppSettings,
    parent: &Path,
    contents: BackupContents,
) -> Result<PathBuf, String> {
    let db = settings.resolve_db_path();
    let data = db.parent().ok_or("Database has no parent directory")?;
    let source = fs::canonicalize(data).map_err(|e| e.to_string())?;
    let parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    for cache in ["images", "file_cache"] {
        if parent.starts_with(source.join(cache)) {
            return Err("Choose a backup location outside the image/file cache".into());
        }
    }
    let target = parent.join(format!(
        "clippi-backup-{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&target).map_err(|e| e.to_string())?;
    let result = (|| {
        if contents.settings {
            settings.save_atomic_to(&target.join("clippi.toml"))?;
        }
        if contents.data {
            let _cache_guard = super::data_operation::lock_cache_io();
            let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| e.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|e| e.to_string())?;
            // VACUUM INTO captures a consistent SQLite snapshot, including WAL data.
            conn.execute(
                "VACUUM INTO ?1",
                [target.join("clippi.db").to_string_lossy().as_ref()],
            )
            .map_err(|e| e.to_string())?;
            for cache in ["images", "file_cache"] {
                if data.join(cache).exists() {
                    copy_tree(&data.join(cache), &target.join(cache))?;
                } else {
                    fs::create_dir(target.join(cache)).map_err(|e| e.to_string())?;
                }
            }
        }
        let mut files = BTreeMap::new();
        inventory(&target, &target, &mut files)?;
        let manifest = Manifest {
            version: 3,
            platform: std::env::consts::OS.to_owned(),
            data_dir: data.to_path_buf(),
            files,
            contents,
        };
        fs::write(
            target.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(target.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&target);
    }
    result
}

fn remap(value: &str, old: &Path, new: &Path) -> String {
    let path = super::paths::without_verbatim_prefix(Path::new(value));
    let old = super::paths::without_verbatim_prefix(old);
    for cache in ["images", "file_cache"] {
        if let Ok(relative) = path.strip_prefix(old.join(cache)) {
            // Reject traversal rather than rewriting a path outside the cache.
            if relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                let mut target = new.join(cache);
                for component in relative.components() {
                    target.push(component.as_os_str());
                }
                return target.to_string_lossy().into_owned();
            }
        }
    }
    value.to_owned()
}

fn remap_json(value: &mut serde_json::Value, old: &Path, new: &Path) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "path" | "local_path" | "cache_path") {
                    if let Some(path) = value.as_str() {
                        *value = remap(path, old, new).into();
                    }
                } else {
                    remap_json(value, old, new);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                remap_json(value, old, new);
            }
        }
        _ => {}
    }
}

/// Check the full archive inventory and its advertised contents.
fn validate_snapshot(source: &Path) -> Result<Manifest, String> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(source.join("manifest.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if manifest.version != 3 || manifest.contents.is_empty() {
        return Err("Unsupported backup version".into());
    }
    // Local archives contain native paths and platform-specific preferences.
    // Do not guess the platform of archives created before this metadata existed.
    if manifest.platform != std::env::consts::OS {
        return Err(PLATFORM_MISMATCH.into());
    }
    let mut files = BTreeMap::new();
    inventory(source, source, &mut files)?;
    if files != manifest.files
        || (manifest.contents.data && !files.contains_key("clippi.db"))
        || (manifest.contents.settings && !files.contains_key("clippi.toml"))
    {
        return Err("Backup is incomplete or its files have changed".into());
    }
    Ok(manifest)
}

fn restore_folder_selected(
    source: &Path,
    current: &AppSettings,
    selected: BackupContents,
) -> Result<AppSettings, String> {
    let source = fs::canonicalize(source).map_err(|e| e.to_string())?;
    let manifest = validate_snapshot(&source)?;
    selected.validate_selection(manifest.contents)?;
    let mut settings: AppSettings = if selected.settings {
        toml::from_str(&fs::read_to_string(source.join("clippi.toml")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        current.clone()
    };
    // Storage location belongs to the active data, never to settings alone.
    settings.db_path = current.db_path.clone();
    if !selected.data {
        return Ok(settings);
    }
    let db = current.resolve_db_path();
    let parent = db.parent().ok_or("Database has no parent directory")?;
    let parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
    // Never stage inside the source tree, even when the active database was
    // manually pointed at a backup folder.
    let parent = if parent.starts_with(&source) {
        source.parent().ok_or("Backup has no parent directory")?
    } else {
        parent.as_path()
    };
    let target = super::paths::without_verbatim_prefix(parent)
        .join(format!("clippi-restored-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&target).map_err(|e| e.to_string())?;
    let result = (|| {
        copy_tree(&source, &target)?;
        let mut staged_files = BTreeMap::new();
        inventory(&target, &target, &mut staged_files)?;
        if staged_files != manifest.files {
            return Err("Backup changed while restoring".into());
        }
        // Only the selected data is retained in the restored directory.
        if target.join("clippi.toml").exists() {
            fs::remove_file(target.join("clippi.toml")).map_err(|e| e.to_string())?;
        }
        fs::remove_file(target.join("manifest.json")).map_err(|e| e.to_string())?;
        let conn = Connection::open(target.join("clippi.db")).map_err(|e| e.to_string())?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if integrity != "ok" {
            return Err(format!("Backup database is corrupt: {integrity}"));
        }
        let rows = {
            let mut stmt = conn
                .prepare("SELECT id, image_path, full_text, file_data FROM clipboard_items")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?
        };
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| e.to_string())?;
        for (id, image, text, file_data) in rows {
            let new_image = remap(&image, &manifest.data_dir, &target);
            let text = if !image.is_empty() && text == image {
                new_image.clone()
            } else {
                text
            };
            let file_data = if file_data.is_empty() {
                file_data
            } else {
                let mut json: serde_json::Value =
                    serde_json::from_str(&file_data).map_err(|e| e.to_string())?;
                remap_json(&mut json, &manifest.data_dir, &target);
                json.to_string()
            };
            conn.execute(
                "UPDATE clipboard_items SET image_path=?1, full_text=?2, file_data=?3 WHERE id=?4",
                params![new_image, text, file_data, id],
            )
            .map_err(|e| e.to_string())?;
        }
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| e.to_string())?;
        settings.db_path = target.join("clippi.db").to_string_lossy().into_owned();
        Ok(settings)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&target);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selective_fixture() -> (tempfile::TempDir, AppSettings, AppSettings) {
        let root = tempfile::tempdir().unwrap();
        let backup_dir = root.path().join("backup-data");
        let current_dir = root.path().join("current-data");
        fs::create_dir_all(backup_dir.join("images")).unwrap();
        fs::create_dir_all(backup_dir.join("file_cache")).unwrap();
        fs::create_dir_all(&current_dir).unwrap();
        fs::write(backup_dir.join("images").join("image.png"), b"image").unwrap();
        fs::write(backup_dir.join("file_cache").join("file.txt"), b"file").unwrap();
        let backup = AppSettings {
            db_path: backup_dir.join("clippi.db").to_string_lossy().into_owned(),
            max_items: 543,
            ..AppSettings::default()
        };
        let current = AppSettings {
            db_path: current_dir.join("clippi.db").to_string_lossy().into_owned(),
            max_items: 987,
            ..AppSettings::default()
        };
        for (settings, text) in [(&backup, "backup history"), (&current, "current history")] {
            let database = super::super::db::Database::open(&settings.db_path).unwrap();
            let connection = Connection::open(&settings.db_path).unwrap();
            connection.execute("INSERT INTO clipboard_items (full_text, content_hash, created_at, updated_at) VALUES (?1, 42, 'now', 'now')", [text]).unwrap();
            drop(connection);
            drop(database);
        }
        (root, backup, current)
    }

    fn history(settings: &AppSettings) -> String {
        Connection::open(&settings.db_path)
            .unwrap()
            .query_row("SELECT full_text FROM clipboard_items", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn one_archive_supports_selective_restore_without_changing_unselected_contents() {
        let (root, backup, current) = selective_fixture();
        let zip = root.path().join("full.zip");
        create(&backup, &zip).unwrap();
        let prepared = prepare_restore(&zip).unwrap();
        assert_eq!(prepared.contents(), BackupContents::default());
        let settings_only = prepared
            .restore(
                &current,
                BackupContents {
                    settings: true,
                    data: false,
                },
            )
            .unwrap();
        assert_eq!(settings_only.max_items, backup.max_items);
        assert_eq!(settings_only.db_path, current.db_path);
        assert_eq!(history(&settings_only), "current history");
        let data_only = prepared
            .restore(
                &current,
                BackupContents {
                    settings: false,
                    data: true,
                },
            )
            .unwrap();
        assert_eq!(data_only.max_items, current.max_items);
        assert_eq!(history(&data_only), "backup history");
        let data_dir = Path::new(&data_only.db_path).parent().unwrap();
        assert_eq!(
            fs::read(data_dir.join("images").join("image.png")).unwrap(),
            b"image"
        );
        assert_eq!(
            fs::read(data_dir.join("file_cache").join("file.txt")).unwrap(),
            b"file"
        );
        assert!(!data_dir.join("clippi.toml").exists());
        let both = prepared
            .restore(&current, BackupContents::default())
            .unwrap();
        assert_eq!(both.max_items, backup.max_items);
        assert_eq!(history(&both), "backup history");
        assert_eq!(history(&current), "current history");
        assert_eq!(history(&backup), "backup history");
    }

    #[test]
    fn partial_archives_only_include_selected_payloads_and_reject_unavailable_selections() {
        let (root, backup, current) = selective_fixture();
        for contents in [
            BackupContents {
                settings: true,
                data: false,
            },
            BackupContents {
                settings: false,
                data: true,
            },
        ] {
            let zip = root
                .path()
                .join(format!("partial-{}.zip", contents.settings));
            create_selected(&backup, &zip, contents).unwrap();
            let mut archive = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
            assert_eq!(archive.by_name("clippi.toml").is_ok(), contents.settings);
            assert_eq!(archive.by_name("clippi.db").is_ok(), contents.data);
            assert_eq!(archive.by_name("images/image.png").is_ok(), contents.data);
            drop(archive);
            let prepared = prepare_restore(&zip).unwrap();
            assert_eq!(prepared.contents(), contents);
            let restored = prepared.restore(&current, contents).unwrap();
            assert_eq!(
                restored.max_items,
                if contents.settings {
                    backup.max_items
                } else {
                    current.max_items
                }
            );
            assert_eq!(
                history(&restored),
                if contents.data {
                    "backup history"
                } else {
                    "current history"
                }
            );
            assert!(prepared
                .restore(&current, BackupContents::default())
                .is_err());
            assert!(prepared
                .restore(
                    &current,
                    BackupContents {
                        settings: false,
                        data: false
                    }
                )
                .is_err());
        }
        let empty = root.path().join("empty.zip");
        assert!(create_selected(
            &backup,
            &empty,
            BackupContents {
                settings: false,
                data: false
            }
        )
        .is_err());
        assert!(!empty.exists());
    }

    #[test]
    fn foreign_and_unknown_platforms_are_rejected_before_restore() {
        let (root, backup, current) = selective_fixture();
        let snapshot = create_folder(&backup, root.path()).unwrap();
        let manifest_path = snapshot.join("manifest.json");
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(original["platform"], std::env::consts::OS);
        assert_eq!(original["version"], 3);
        for platform in [
            Some("windows"),
            Some("macos"),
            Some("linux"),
            Some(""),
            None,
        ] {
            if platform == Some(std::env::consts::OS) {
                continue;
            }
            let mut manifest = original.clone();
            if let Some(platform) = platform {
                manifest["platform"] = platform.into();
            } else {
                manifest.as_object_mut().unwrap().remove("platform");
            }
            fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let zip = root.path().join("foreign.zip");
            let mut archive = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
            zip_tree(&snapshot, &snapshot, &mut archive).unwrap();
            archive.finish().unwrap();
            assert_eq!(prepare_restore(&zip).err().unwrap(), PLATFORM_MISMATCH);
            for selected in [
                BackupContents::default(),
                BackupContents {
                    settings: true,
                    data: false,
                },
                BackupContents {
                    settings: false,
                    data: true,
                },
            ] {
                assert_eq!(
                    restore_folder_selected(&snapshot, &current, selected).unwrap_err(),
                    PLATFORM_MISMATCH
                );
            }
            assert_eq!(history(&current), "current history");
            assert!(fs::read_dir(Path::new(&current.db_path).parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("clippi-restored-")));
        }
    }

    #[test]
    fn legacy_archives_without_platform_metadata_are_rejected() {
        let (root, backup, _) = selective_fixture();
        let snapshot = create_folder(&backup, root.path()).unwrap();
        let manifest_path = snapshot.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.as_object_mut().unwrap().remove("platform");
        for version in [1, 2, 4] {
            manifest["version"] = version.into();
            fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            assert!(validate_snapshot(&snapshot).is_err());
        }
    }

    #[test]
    fn full_backup_restores_wal_settings_and_cached_paths_without_overwriting() {
        let root =
            std::env::temp_dir().join(format!("clippi-backup-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("images/icons")).unwrap();
        fs::create_dir_all(root.join("file_cache/hash")).unwrap();
        let image = root.join("images/picture.png");
        let cached_file = root.join("file_cache/hash/document.txt");
        fs::write(&image, b"image payload").unwrap();
        fs::write(&cached_file, b"file payload").unwrap();
        fs::write(root.join("images/icons/app.png"), b"icon payload").unwrap();
        let settings = AppSettings {
            db_path: root.join("live.db").to_string_lossy().into_owned(),
            max_items: 543,
            ..AppSettings::default()
        };
        let database = super::super::db::Database::open(&settings.db_path).unwrap();
        let writer = Connection::open(&settings.db_path).unwrap();
        writer.execute_batch("PRAGMA wal_autocheckpoint=0").unwrap();
        let file_data = serde_json::json!({"files": [{"path": cached_file, "name": "document.txt", "is_dir": false}]}).to_string();
        writer.execute("INSERT INTO clipboard_items (content_type, full_text, content_hash, created_at, updated_at, image_path, file_data) VALUES ('image', ?1, 42, 'now', 'now', ?1, ?2)", params![image.to_string_lossy(), file_data]).unwrap();
        let backup = root.join("backup.zip");
        create(&settings, &backup).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(&backup).unwrap()).unwrap();
        assert_eq!(
            archive.by_name("clippi.db").unwrap().compression(),
            zip::CompressionMethod::Deflated
        );
        assert!(archive.by_name("images/icons/app.png").is_ok());
        drop(archive);
        writer
            .execute("UPDATE clipboard_items SET note='after backup'", [])
            .unwrap();
        let restored = restore(&backup, &settings).unwrap();
        assert_eq!(restored.max_items, 543);
        assert_ne!(restored.db_path, settings.db_path);
        assert!(!restored.db_path.starts_with(r"\\?\"));
        let restored_dir = Path::new(&restored.db_path).parent().unwrap();
        assert_eq!(
            fs::read(restored_dir.join("images/picture.png")).unwrap(),
            b"image payload"
        );
        assert_eq!(
            fs::read(restored_dir.join("images/icons/app.png")).unwrap(),
            b"icon payload"
        );
        assert_eq!(
            fs::read(restored_dir.join("file_cache/hash/document.txt")).unwrap(),
            b"file payload"
        );
        let reader = Connection::open(&restored.db_path).unwrap();
        let (path, text, json, note): (String, String, String, String) = reader
            .query_row(
                "SELECT image_path, full_text, file_data, note FROM clipboard_items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            path,
            restored_dir
                .join("images")
                .join("picture.png")
                .to_string_lossy()
        );
        assert_eq!(path, text);
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            json["files"][0]["path"].as_str().unwrap(),
            restored_dir
                .join("file_cache")
                .join("hash")
                .join("document.txt")
                .to_string_lossy()
        );
        assert!(note.is_empty());
        assert_eq!(
            writer
                .query_row::<String, _, _>("SELECT note FROM clipboard_items", [], |r| r.get(0))
                .unwrap(),
            "after backup"
        );
        drop(reader);
        drop(writer);
        drop(database);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_backup_is_rejected_before_staging_and_cache_destination_is_rejected() {
        let root =
            std::env::temp_dir().join(format!("clippi-backup-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("images")).unwrap();
        let settings = AppSettings {
            db_path: root.join("live.db").to_string_lossy().into_owned(),
            ..AppSettings::default()
        };
        let database = super::super::db::Database::open(&settings.db_path).unwrap();
        assert!(create(&settings, &root.join("images").join("backup.zip")).is_err());
        assert!(create(&settings, Path::new(&settings.db_path)).is_err());
        let backup = root.join("backup.zip");
        create(&settings, &backup).unwrap();
        fs::write(&backup, b"corrupt").unwrap();
        assert!(restore(&backup, &settings).is_err());
        assert!(fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("clippi-restored-")));
        drop(database);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zip_traversal_is_rejected_without_writing_outside_staging() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let source = root.path().join("unsafe.zip");
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        archive
            .start_file("../escaped.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"unsafe").unwrap();
        archive.finish().unwrap();
        assert!(extract(&source, &staging).is_err());
        assert!(!root.path().join("escaped.txt").exists());
    }

    #[test]
    fn changed_archive_payload_is_rejected_and_failed_backup_preserves_destination() {
        let root = tempfile::tempdir().unwrap();
        let settings = AppSettings {
            db_path: root.path().join("live.db").to_string_lossy().into_owned(),
            ..AppSettings::default()
        };
        let database = super::super::db::Database::open(&settings.db_path).unwrap();
        let snapshot = create_folder(&settings, root.path()).unwrap();
        fs::write(snapshot.join("clippi.toml"), "changed").unwrap();
        let source = root.path().join("changed.zip");
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        zip_tree(&snapshot, &snapshot, &mut archive).unwrap();
        archive.finish().unwrap();
        assert!(restore(&source, &settings).is_err());
        let destination = root.path().join("existing.zip");
        fs::write(&destination, b"previous backup").unwrap();
        let missing = AppSettings {
            db_path: root
                .path()
                .join("missing.db")
                .to_string_lossy()
                .into_owned(),
            ..settings
        };
        assert!(create(&missing, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"previous backup");
        drop(database);
    }
}
